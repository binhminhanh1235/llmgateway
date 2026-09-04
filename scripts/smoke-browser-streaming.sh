#!/usr/bin/env bash
set -euo pipefail

export LLMGATEWAY_API_KEY="ci-local-key"
export FAKE_API_KEY="fake-key"
export LLMGATEWAY_CONFIG="/tmp/llmgateway-browser-streaming-smoke.toml"
PROFILE_ROOT="/tmp/llmgateway-browser-streaming-profiles"
FAKE_CHROMIUM="/tmp/llmgateway-fake-streaming-chromium"
BROWSER_PID=""

rm -rf "$PROFILE_ROOT"
rm -f data/llmgateway.db data/llmgateway.db-shm data/llmgateway.db-wal
mkdir -p data

cat >"$FAKE_CHROMIUM" <<'SH'
#!/usr/bin/env bash
exec python3 scripts/fake-cdp-chromium.py "$@"
SH
chmod 700 "$FAKE_CHROMIUM"

cat >"$LLMGATEWAY_CONFIG" <<EOF
[server]
host = "127.0.0.1"
port = 7331

[api]
key_env = "LLMGATEWAY_API_KEY"
default_model = "llmgateway-auto"

[storage]
database_url = "sqlite://data/llmgateway.db"

[browser]
enabled = false
profile_root = "$PROFILE_ROOT"

[chromium]
enabled = false
executable = "$FAKE_CHROMIUM"
startup_timeout_seconds = 5
auto_recover = true
reconcile_interval_seconds = 15
extra_args = []

[context]
enabled = false
retrieval_enabled = false

[[providers]]
id = "fake-api"
kind = "openai-compatible"
base_url = "http://127.0.0.1:18080/v1"
models_path = "models"

[[accounts]]
id = "api-account"
provider = "fake-api"
api_key_env = "FAKE_API_KEY"
enabled = true
discover_models = false

[[routes]]
id = "api-route"
account = "api-account"
model = "fake-model"
priority = 20
enabled = true
capabilities = ["chat"]

[virtual_models.llmgateway-auto]
routes = ["api-route"]

[virtual_models.llmgateway-coding]
routes = ["api-route"]

[virtual_models.llmgateway-best]
routes = ["api-route"]
EOF

python3 scripts/fake-openai.py >/tmp/llmgateway-browser-streaming-api.log 2>&1 &
FAKE_PID=$!
cargo build --quiet
./target/debug/llmgateway >/tmp/llmgateway-browser-streaming.log 2>&1 &
PID=$!

cleanup() {
  if [ -n "$BROWSER_PID" ]; then kill "$BROWSER_PID" 2>/dev/null || true; fi
  kill "$PID" "$FAKE_PID" 2>/dev/null || true
  rm -rf "$PROFILE_ROOT"
  rm -f "$FAKE_CHROMIUM" "$LLMGATEWAY_CONFIG"
  rm -f data/llmgateway.db data/llmgateway.db-shm data/llmgateway.db-wal
}
trap cleanup EXIT

for _ in {1..60}; do
  curl -fsS http://127.0.0.1:7331/_llmgateway/health >/dev/null && break
  sleep 0.2
done

AUTH=(-H "Authorization: Bearer ${LLMGATEWAY_API_KEY}")
JSON=(-H "Content-Type: application/json")

curl -fsS -X POST http://127.0.0.1:7331/_llmgateway/browser-account-setup   "${AUTH[@]}" "${JSON[@]}"   -d '{"provider":"qwen","account_id":"qwen-stream","label":"Qwen Stream CI","priority":5}'   >/tmp/llmgateway-browser-streaming-create.json

LAUNCH=$(curl -fsS -X POST   http://127.0.0.1:7331/_llmgateway/browser-sessions/qwen-stream/driver/launch   "${AUTH[@]}")
BROWSER_PID=$(printf '%s' "$LAUNCH" | python3 -c 'import json,sys; print(json.load(sys.stdin)["launch"]["pid"] or "")')
PROFILE_DIR=$(printf '%s' "$LAUNCH" | python3 -c 'import json,sys; print(json.load(sys.stdin)["launch"]["profile_dir"])')
export PROFILE_DIR

curl -fsS -X POST   http://127.0.0.1:7331/_llmgateway/browser-sessions/qwen-stream/driver/verify   "${AUTH[@]}" >/tmp/llmgateway-browser-streaming-verify.json

python3 <<'PY'
import http.client
import json
import os
import time

API_KEY = os.environ["LLMGATEWAY_API_KEY"]

def request_stream(path, payload):
    conn = http.client.HTTPConnection("127.0.0.1", 7331, timeout=10)
    started = time.monotonic()
    body = json.dumps(payload)
    conn.request(
        "POST",
        path,
        body=body,
        headers={
            "Authorization": f"Bearer {API_KEY}",
            "Content-Type": "application/json",
            "Content-Length": str(len(body.encode())),
        },
    )
    response = conn.getresponse()
    assert response.status == 200, (response.status, response.read())
    assert response.getheader("x-llmgateway-route") == "qwen-stream-route", dict(response.getheaders())
    events = []
    first_data_at = None
    while True:
        line = response.readline()
        if not line:
            break
        if not line.startswith(b"data: "):
            continue
        data = line[6:].strip()
        if data == b"[DONE]":
            break
        if not data:
            continue
        if first_data_at is None:
            first_data_at = time.monotonic() - started
        events.append(json.loads(data))
    total = time.monotonic() - started
    conn.close()
    return events, first_data_at, total

chat, first, total = request_stream(
    "/v1/chat/completions",
    {
        "model": "llmgateway-auto",
        "stream": True,
        "messages": [{"role": "user", "content": "stream through browser"}],
    },
)
text = "".join(
    choice.get("delta", {}).get("content", "")
    for event in chat
    for choice in event.get("choices", [])
)
assert text == "browser-stream-ok", (text, chat)
assert first is not None and first < 0.8, (first, total)
assert total >= 1.0, (first, total)
assert total - first >= 0.6, (first, total)

responses, _, _ = request_stream(
    "/v1/responses",
    {
        "model": "llmgateway-auto",
        "stream": True,
        "input": "stream through responses",
    },
)
responses_text = "".join(
    event.get("delta", "")
    for event in responses
    if event.get("type") == "response.output_text.delta"
)
assert responses_text == "browser-stream-ok", (responses_text, responses)
assert any(event.get("type") == "response.completed" for event in responses), responses

anthropic, _, _ = request_stream(
    "/v1/messages",
    {
        "model": "llmgateway-auto",
        "stream": True,
        "max_tokens": 128,
        "messages": [{"role": "user", "content": "stream through anthropic"}],
    },
)
anthropic_text = "".join(
    event.get("delta", {}).get("text", "")
    for event in anthropic
    if event.get("type") == "content_block_delta"
    and event.get("delta", {}).get("type") == "text_delta"
)
assert anthropic_text == "browser-stream-ok", (anthropic_text, anthropic)
assert any(event.get("type") == "message_stop" for event in anthropic), anthropic

profile = os.environ["PROFILE_DIR"]
for marker in ("stream-cancelled", "target-closed"):
    try:
        os.remove(os.path.join(profile, marker))
    except FileNotFoundError:
        pass

conn = http.client.HTTPConnection("127.0.0.1", 7331, timeout=10)
payload = json.dumps({
    "model": "llmgateway-auto",
    "stream": True,
    "messages": [{"role": "user", "content": "cancel after first browser chunk"}],
})
conn.request(
    "POST",
    "/v1/chat/completions",
    body=payload,
    headers={
        "Authorization": f"Bearer {API_KEY}",
        "Content-Type": "application/json",
        "Content-Length": str(len(payload.encode())),
    },
)
response = conn.getresponse()
assert response.status == 200
while True:
    line = response.readline()
    if line.startswith(b"data: {"):
        break
    if not line:
        raise AssertionError("browser stream ended before first data chunk")
conn.close()
PY

for _ in {1..50}; do
  if [ -f "$PROFILE_DIR/stream-cancelled" ] && [ -f "$PROFILE_DIR/target-closed" ]; then
    break
  fi
  sleep 0.1
done

test -f "$PROFILE_DIR/stream-cancelled"
test -f "$PROFILE_DIR/target-closed"
test "$(cat "$PROFILE_DIR/stream-poll-count")" -ge 1

curl -fsS -X POST   http://127.0.0.1:7331/_llmgateway/browser-sessions/qwen-stream/driver/stop   "${AUTH[@]}" >/dev/null
BROWSER_PID=""

echo "llmgateway v0.30 true browser streaming + cancellation smoke test passed"
