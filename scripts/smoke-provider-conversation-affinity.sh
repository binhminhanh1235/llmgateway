#!/usr/bin/env bash
set -euo pipefail

export LLMGATEWAY_API_KEY="ci-provider-conversation-key"
export FAKE_API_KEY="fake-key"
export LLMGATEWAY_CONFIG="/tmp/llmgateway-provider-conversation-affinity.toml"
PROFILE_ROOT="/tmp/llmgateway-provider-conversation-affinity-profiles"
FAKE_CHROMIUM="/tmp/llmgateway-fake-affinity-chromium"
LAUNCH_LOG="/tmp/llmgateway-provider-conversation-launches.log"
BROWSER_PID=""

rm -rf "$PROFILE_ROOT"
rm -f "$LAUNCH_LOG"
rm -f data/llmgateway.db data/llmgateway.db-shm data/llmgateway.db-wal
mkdir -p data

cat >"$FAKE_CHROMIUM" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >>/tmp/llmgateway-provider-conversation-launches.log
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
enabled = true
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

python3 scripts/fake-openai.py >/tmp/llmgateway-provider-conversation-api.log 2>&1 &
FAKE_PID=$!
cargo build --quiet
./target/debug/llmgateway >/tmp/llmgateway-provider-conversation.log 2>&1 &
PID=$!

cleanup() {
  if [ -n "$BROWSER_PID" ]; then kill "$BROWSER_PID" 2>/dev/null || true; fi
  kill "$PID" "$FAKE_PID" 2>/dev/null || true
  rm -rf "$PROFILE_ROOT"
  rm -f "$FAKE_CHROMIUM" "$LLMGATEWAY_CONFIG" "$LAUNCH_LOG"
  rm -f data/llmgateway.db data/llmgateway.db-shm data/llmgateway.db-wal
}
trap cleanup EXIT

for _ in {1..60}; do
  curl -fsS http://127.0.0.1:7331/_llmgateway/health >/dev/null && break
  sleep 0.2
done

AUTH=(-H "Authorization: Bearer ${LLMGATEWAY_API_KEY}")
JSON=(-H "Content-Type: application/json")

curl -fsS -X POST http://127.0.0.1:7331/_llmgateway/browser-account-setup   "${AUTH[@]}" "${JSON[@]}"   -d '{"provider":"gemini","account_id":"gemini-affinity","label":"Gemini Affinity CI","priority":5}'   >/tmp/llmgateway-provider-conversation-create.json

LAUNCH=$(curl -fsS -X POST   http://127.0.0.1:7331/_llmgateway/browser-sessions/gemini-affinity/driver/launch   "${AUTH[@]}")
BROWSER_PID=$(printf '%s' "$LAUNCH" | python3 -c 'import json,sys; print(json.load(sys.stdin)["launch"]["pid"] or "")')
PROFILE_DIR=$(printf '%s' "$LAUNCH" | python3 -c 'import json,sys; print(json.load(sys.stdin)["launch"]["profile_dir"])')
export PROFILE_DIR

curl -fsS -X POST   http://127.0.0.1:7331/_llmgateway/browser-sessions/gemini-affinity/driver/verify   "${AUTH[@]}" >/tmp/llmgateway-provider-conversation-verify.json
python3 <<'PY'
import json
with open("/tmp/llmgateway-provider-conversation-verify.json", encoding="utf-8") as f:
    verify = json.load(f)
assert verify["authenticated"] is True, verify
assert verify["browser_closed_after_capture"] is True, verify
assert verify["status"]["running"] is False, verify
PY
BROWSER_PID=""

curl -fsS \
  http://127.0.0.1:7331/_llmgateway/browser-accounts/gemini-affinity/runtime \
  "${AUTH[@]}" >/tmp/llmgateway-provider-runtime.json

python3 <<'PY'
import json
with open("/tmp/llmgateway-provider-runtime.json", encoding="utf-8") as f:
    runtime = json.load(f)
assert runtime["account_id"] == "gemini-affinity", runtime
assert runtime["session"]["status"] == "ready", runtime
assert runtime["browser_running"] is False, runtime
assert runtime["effective_transport"] == "unavailable", runtime
assert runtime["auth_snapshot_available"] is False, runtime
PY

# P5: ordinary fresh requests use authenticated browser-context fetch before UI.
# The fake CDP models provider fetch separately from DOM/native conversation actions.
curl -fsS -D /tmp/browser-fetch-buffered.headers -o /tmp/browser-fetch-buffered.json \
  -X POST http://127.0.0.1:7331/v1/chat/completions \
  "${AUTH[@]}" "${JSON[@]}" \
  -d '{"model":"llmgateway-auto","stream":false,"messages":[{"role":"user","content":"p5 buffered browser fetch"}]}'
grep -qi '^x-llmgateway-route: gemini-affinity-route' /tmp/browser-fetch-buffered.headers
python3 <<'PY'
import json
with open("/tmp/browser-fetch-buffered.json", encoding="utf-8") as f:
    body = json.load(f)
assert body["choices"][0]["message"]["content"] == "browser-fetch-ok", body
PY

curl -fsS \
  http://127.0.0.1:7331/_llmgateway/browser-accounts/gemini-affinity/runtime \
  "${AUTH[@]}" >/tmp/llmgateway-browser-fetch-runtime.json
python3 <<'PY'
import json
with open("/tmp/llmgateway-browser-fetch-runtime.json", encoding="utf-8") as f:
    runtime = json.load(f)
last = runtime.get("last_execution")
assert last is not None, runtime
assert last["transport"] == "browser-fetch", runtime
assert last["browser_fallback"] is False, runtime
assert runtime["browser_running"] is True, runtime
PY

# Automatic P5 execution must reuse P4 invisible runtime policy.
test "$(wc -l < "$LAUNCH_LOG" | tr -d ' ')" -ge 2
AUTO_LAUNCH=$(tail -n 1 "$LAUNCH_LOG")
printf '%s' "$AUTO_LAUNCH" | grep -q -- '--headless'
if printf '%s' "$AUTO_LAUNCH" | grep -q -- '--new-window'; then
  echo "browser-fetch background execution unexpectedly launched a visible Chromium window" >&2
  exit 1
fi

curl -fsS -N -D /tmp/browser-fetch-stream.headers -o /tmp/browser-fetch-stream.sse \
  -X POST http://127.0.0.1:7331/v1/chat/completions \
  "${AUTH[@]}" "${JSON[@]}" \
  -d '{"model":"llmgateway-auto","stream":true,"messages":[{"role":"user","content":"p5 streaming browser fetch"}]}'
grep -qi '^x-llmgateway-route: gemini-affinity-route' /tmp/browser-fetch-stream.headers
python3 <<'PY'
import json
chunks = []
done = False
with open("/tmp/browser-fetch-stream.sse", encoding="utf-8") as f:
    for line in f:
        if not line.startswith("data: "):
            continue
        raw = line[6:].strip()
        if raw == "[DONE]":
            done = True
            continue
        if not raw:
            continue
        event = json.loads(raw)
        delta = (event.get("choices") or [{}])[0].get("delta") or {}
        chunks.append(str(delta.get("content") or ""))
assert "".join(chunks) == "browser-stream-ok", chunks
assert done is True, chunks
PY

python3 <<'PY'
import json
import os
profile = os.environ["PROFILE_DIR"]
with open(os.path.join(profile, "browser-fetch-requests.jsonl"), encoding="utf-8") as f:
    requests = [json.loads(line) for line in f if line.strip()]
assert len(requests) >= 2, requests
assert any(
    message.get("content") == "p5 buffered browser fetch"
    for message in requests[-2].get("messages", [])
), requests[-2]
assert any(
    message.get("content") == "p5 streaming browser fetch"
    for message in requests[-1].get("messages", [])
), requests[-1]
ui_path = os.path.join(profile, "browser-requests.jsonl")
if os.path.exists(ui_path):
    with open(ui_path, encoding="utf-8") as f:
        ui_requests = [line for line in f if line.strip()]
    assert not ui_requests, ui_requests
PY

curl -fsS -X POST \
  http://127.0.0.1:7331/_llmgateway/browser-sessions/gemini-affinity/driver/verify \
  "${AUTH[@]}" >/tmp/llmgateway-browser-fetch-close.json
python3 <<'PY'
import json
with open("/tmp/llmgateway-browser-fetch-close.json", encoding="utf-8") as f:
    verify = json.load(f)
assert verify["authenticated"] is True, verify
assert verify["browser_closed_after_capture"] is True, verify
assert verify["status"]["running"] is False, verify
PY
BROWSER_PID=""

THREAD_A=$(curl -fsS -X POST http://127.0.0.1:7331/v1/threads   "${AUTH[@]}" "${JSON[@]}"   -d '{"title":"Affinity A","model":"llmgateway-auto"}'   | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')

curl -fsS -D /tmp/affinity-a1.headers -o /tmp/affinity-a1.json   -X POST "http://127.0.0.1:7331/v1/threads/$THREAD_A/messages"   "${AUTH[@]}" "${JSON[@]}"   -d '{"content":"alpha-one","stream":true}'
grep -qi '^x-llmgateway-route: gemini-affinity-route' /tmp/affinity-a1.headers

# P4: first ordinary request reopens the authenticated profile headlessly, never visibly.
curl -fsS \
  http://127.0.0.1:7331/_llmgateway/browser-sessions/gemini-affinity/driver/status \
  "${AUTH[@]}" >/tmp/llmgateway-provider-conversation-headless-status.json
python3 <<'PY'
import json
with open("/tmp/llmgateway-provider-conversation-headless-status.json", encoding="utf-8") as f:
    status = json.load(f)
assert status["running"] is True, status
assert status["debugger_reachable"] is True, status
PY
BROWSER_PID=$(python3 -c 'import json; print(json.load(open("/tmp/llmgateway-provider-conversation-headless-status.json"))["pid"] or "")')

curl -fsS -D /tmp/affinity-a2.headers -o /tmp/affinity-a2.json   -X POST "http://127.0.0.1:7331/v1/threads/$THREAD_A/messages"   "${AUTH[@]}" "${JSON[@]}"   -d '{"content":"alpha-two","stream":true}'
grep -qi '^x-llmgateway-route: gemini-affinity-route' /tmp/affinity-a2.headers

THREAD_B=$(curl -fsS -X POST http://127.0.0.1:7331/v1/threads   "${AUTH[@]}" "${JSON[@]}"   -d '{"title":"Affinity B","model":"llmgateway-auto"}'   | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')

curl -fsS -D /tmp/affinity-b1.headers -o /tmp/affinity-b1.json   -X POST "http://127.0.0.1:7331/v1/threads/$THREAD_B/messages"   "${AUTH[@]}" "${JSON[@]}"   -d '{"content":"beta-one","stream":true}'
grep -qi '^x-llmgateway-route: gemini-affinity-route' /tmp/affinity-b1.headers

curl -fsS \
  http://127.0.0.1:7331/_llmgateway/browser-accounts/gemini-affinity/runtime \
  "${AUTH[@]}" >/tmp/llmgateway-provider-runtime-after-chat.json
python3 <<'PY'
import json
with open("/tmp/llmgateway-provider-runtime-after-chat.json", encoding="utf-8") as f:
    runtime = json.load(f)
last = runtime.get("last_execution")
assert last is not None, runtime
assert last["transport"] == "browser-cdp", runtime
assert last["browser_fallback"] is False, runtime
PY

curl -fsS \
  "http://127.0.0.1:7331/_llmgateway/threads/$THREAD_A/browser-affinity/gemini-affinity" \
  "${AUTH[@]}" >/tmp/llmgateway-affinity-a.json
curl -fsS \
  "http://127.0.0.1:7331/_llmgateway/threads/$THREAD_B/browser-affinity/gemini-affinity" \
  "${AUTH[@]}" >/tmp/llmgateway-affinity-b.json

python3 <<'PY'
import json
import os
import sqlite3

profile = os.environ["PROFILE_DIR"]

with open("/tmp/llmgateway-affinity-a.json", encoding="utf-8") as f:
    affinity_a = json.load(f)
with open("/tmp/llmgateway-affinity-b.json", encoding="utf-8") as f:
    affinity_b = json.load(f)
assert affinity_a["mapping"]["conversation_url"] == "https://gemini.google.com/app/ci-thread-1", affinity_a
assert affinity_b["mapping"]["conversation_url"] == "https://gemini.google.com/app/ci-thread-2", affinity_b
assert affinity_a["state"]["present"] is False, affinity_a
assert "conduit_token" not in affinity_a["state"], affinity_a
assert "metadata" not in affinity_a["state"], affinity_a

with open(os.path.join(profile, "opened-targets.log"), encoding="utf-8") as f:
    opened = [line.strip() for line in f if line.strip()]
try:
    with open(os.path.join(profile, "stream-debug.log"), encoding="utf-8") as f:
        stream_debug = [line.strip() for line in f if line.strip()]
except FileNotFoundError:
    stream_debug = []
db = sqlite3.connect("data/llmgateway.db")
all_mappings = db.execute(
    """SELECT thread_id, provider, account_id, conversation_url, last_synced_ordinal
       FROM provider_conversations
       ORDER BY created_at, thread_id"""
).fetchall()

try:
    with open("/tmp/llmgateway-provider-conversation.log", encoding="utf-8") as f:
        gateway_log = f.read().splitlines()[-120:]
except FileNotFoundError:
    gateway_log = []

assert opened[:2] == [
    "https://gemini.google.com/app",
    "https://gemini.google.com/app",
], {"opened": opened, "stream_debug": stream_debug, "mappings": all_mappings, "gateway_log": gateway_log}
assert len(opened) == 2, {"opened": opened, "stream_debug": stream_debug, "mappings": all_mappings, "gateway_log": gateway_log}

with open(os.path.join(profile, "browser-requests.jsonl"), encoding="utf-8") as f:
    requests = [json.loads(line) for line in f if line.strip()]
assert len(requests) >= 3, requests
recent = requests[-3:]
messages = [request["messages"] for request in recent]
assert any(message.get("content") == "alpha-one" for message in messages[0]), messages[0]
assert messages[1] == [{"role": "user", "content": "alpha-two"}], messages[1]
assert messages[2] == [{"role": "user", "content": "beta-one"}], messages[2]
assert recent[0]["target_id"] == recent[1]["target_id"], recent
assert recent[2]["target_id"] != recent[0]["target_id"], recent

rows = db.execute(
    """SELECT thread_id, conversation_url, last_synced_ordinal
       FROM provider_conversations
       WHERE account_id = 'gemini-affinity'
       ORDER BY conversation_url"""
).fetchall()
assert len(rows) == 2, rows
urls = {row[1] for row in rows}
assert urls == {
    "https://gemini.google.com/app/ci-thread-1",
    "https://gemini.google.com/app/ci-thread-2",
}, rows
assert all(row[2] > 0 for row in rows), rows
db.close()
PY

# P7: built-in virtual models must prefer the healthy browser candidate while
# it is available, not just llmgateway-auto.
for VIRTUAL_MODEL in llmgateway-best llmgateway-coding; do
  EXPLAIN=$(curl -fsS -X POST http://127.0.0.1:7331/_llmgateway/routes/explain     "${AUTH[@]}" "${JSON[@]}"     -d "{\"model\":\"$VIRTUAL_MODEL\",\"body\":{\"messages\":[{\"role\":\"user\",\"content\":\"virtual model healthy browser\"}]}}")
  printf '%s' "$EXPLAIN" | python3 -c '
import json,sys
x=json.load(sys.stdin)
assert x["selected_route"] == "gemini-affinity-route", x
candidate=next(c for c in x["candidates"] if c["route_id"] == "gemini-affinity-route")
assert candidate["eligible"] is True, candidate
'
done

# P7: canonical local thread context must survive a provider/account switch.
# Start on Gemini, disable that logical candidate, then continue the same thread
# through the API fallback. The fake API encodes the number of received messages
# in its answer, which proves the fallback received reconstructed local history.
THREAD_C=$(curl -fsS -X POST http://127.0.0.1:7331/v1/threads   "${AUTH[@]}" "${JSON[@]}"   -d '{"title":"Canonical failover","model":"llmgateway-auto"}'   | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')

curl -fsS -D /tmp/affinity-c1.headers -o /tmp/affinity-c1.json   -X POST "http://127.0.0.1:7331/v1/threads/$THREAD_C/messages"   "${AUTH[@]}" "${JSON[@]}"   -d '{"content":"canonical-before-failover","stream":false}'
grep -qi '^x-llmgateway-route: gemini-affinity-route' /tmp/affinity-c1.headers

DISABLE_GEMINI=$(curl -fsS -X PATCH   http://127.0.0.1:7331/_llmgateway/browser-account-setup/gemini-affinity   "${AUTH[@]}" "${JSON[@]}" -d '{"enabled":false}')
printf '%s' "$DISABLE_GEMINI" | python3 -c 'import json,sys; x=json.load(sys.stdin); assert x["enabled"] is False, x'

# P7: virtual-model continuity must reroute to the alternate logical candidate
# instead of treating the browser provider failure as a model failure.
for VIRTUAL_MODEL in llmgateway-best llmgateway-coding; do
  EXPLAIN=$(curl -fsS -X POST http://127.0.0.1:7331/_llmgateway/routes/explain     "${AUTH[@]}" "${JSON[@]}"     -d "{\"model\":\"$VIRTUAL_MODEL\",\"body\":{\"messages\":[{\"role\":\"user\",\"content\":\"virtual model provider failover\"}]}}")
  printf '%s' "$EXPLAIN" | python3 -c '
import json,sys
x=json.load(sys.stdin)
assert x["selected_route"] == "api-route", x
gemini=next(c for c in x["candidates"] if c["route_id"] == "gemini-affinity-route")
assert gemini["eligible"] is False, gemini
assert "account_disabled" in gemini["exclusion_reasons"], gemini
'
done

# P8: when every eligible logical candidate is unavailable, route explain must
# return no selected route rather than looping through transports indefinitely.
curl -fsS -X PATCH http://127.0.0.1:7331/_llmgateway/accounts/api-account   "${AUTH[@]}" "${JSON[@]}" -d '{"enabled":false}' >/tmp/provider-runtime-disable-api.json
python3 - /tmp/provider-runtime-disable-api.json <<'PY'
import json,sys
x=json.load(open(sys.argv[1],encoding="utf-8"))
assert x["enabled"] is False, x
PY
for VIRTUAL_MODEL in llmgateway-auto llmgateway-best llmgateway-coding; do
  EXPLAIN=$(curl -fsS -X POST http://127.0.0.1:7331/_llmgateway/routes/explain     "${AUTH[@]}" "${JSON[@]}"     -d "{\"model\":\"$VIRTUAL_MODEL\",\"body\":{\"messages\":[{\"role\":\"user\",\"content\":\"all candidates unavailable\"}]}}")
  printf '%s' "$EXPLAIN" | python3 -c '
import json,sys
x=json.load(sys.stdin)
assert x["selected_route"] is None, x
assert x["candidates"], x
assert all(not candidate["eligible"] for candidate in x["candidates"]), x
'
done
curl -fsS -X PATCH http://127.0.0.1:7331/_llmgateway/accounts/api-account   "${AUTH[@]}" "${JSON[@]}" -d '{"enabled":true}' >/tmp/provider-runtime-enable-api.json
python3 - /tmp/provider-runtime-enable-api.json <<'PY'
import json,sys
x=json.load(open(sys.argv[1],encoding="utf-8"))
assert x["enabled"] is True, x
PY

curl -fsS -D /tmp/affinity-c2.headers -o /tmp/affinity-c2.json   -X POST "http://127.0.0.1:7331/v1/threads/$THREAD_C/messages"   "${AUTH[@]}" "${JSON[@]}"   -d '{"content":"canonical-after-failover","stream":false}'
grep -qi '^x-llmgateway-route: api-route' /tmp/affinity-c2.headers
python3 <<'PY'
import json
with open("/tmp/affinity-c2.json", encoding="utf-8") as f:
    body = json.load(f)
content = body["choices"][0]["message"]["content"]
assert content.startswith("fake reply messages="), body
count = int(content.rsplit("=", 1)[1])
assert count >= 3, body
PY

ENABLE_GEMINI=$(curl -fsS -X PATCH   http://127.0.0.1:7331/_llmgateway/browser-account-setup/gemini-affinity   "${AUTH[@]}" "${JSON[@]}" -d '{"enabled":true}')
printf '%s' "$ENABLE_GEMINI" | python3 -c 'import json,sys; x=json.load(sys.stdin); assert x["enabled"] is True, x'

echo "provider conversation affinity smoke passed"
