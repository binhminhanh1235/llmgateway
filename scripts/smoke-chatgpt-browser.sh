#!/usr/bin/env bash
set -euo pipefail

export LLMGATEWAY_API_KEY="ci-chatgpt-browser-key"
export FAKE_API_KEY="fake-key"
export LLMGATEWAY_CONFIG="/tmp/llmgateway-chatgpt-browser.toml"
PROFILE_ROOT="/tmp/llmgateway-chatgpt-browser-profiles"
FAKE_CHROMIUM="/tmp/llmgateway-fake-chatgpt-chromium"
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

python3 scripts/fake-openai.py >/tmp/llmgateway-chatgpt-browser-api.log 2>&1 &
FAKE_PID=$!
cargo build --quiet
./target/debug/llmgateway >/tmp/llmgateway-chatgpt-browser.log 2>&1 &
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

CREATE=$(curl -fsS -X POST http://127.0.0.1:7331/_llmgateway/browser-account-setup \
  "${AUTH[@]}" "${JSON[@]}" \
  -d '{"provider":"chatgpt","account_id":"chatgpt-affinity","label":"ChatGPT Affinity CI","priority":5}')
printf '%s' "$CREATE" | python3 -c '
import json,sys
x=json.load(sys.stdin)
assert x["provider"] == "chatgpt", x
assert x["model_id"] == "chatgpt-web-default", x
assert x["restart_required"] is False, x
'

LAUNCH=$(curl -fsS -X POST \
  http://127.0.0.1:7331/_llmgateway/browser-sessions/chatgpt-affinity/driver/launch \
  "${AUTH[@]}")
BROWSER_PID=$(printf '%s' "$LAUNCH" | python3 -c 'import json,sys; print(json.load(sys.stdin)["launch"]["pid"] or "")')
DEBUGGER_PORT=$(printf '%s' "$LAUNCH" | python3 -c 'import json,sys; print(json.load(sys.stdin)["launch"]["debugger_port"])')
PROFILE_DIR=$(printf '%s' "$LAUNCH" | python3 -c 'import json,sys; print(json.load(sys.stdin)["launch"]["profile_dir"])')
export PROFILE_DIR

VERIFY=$(curl -fsS -X POST \
  http://127.0.0.1:7331/_llmgateway/browser-sessions/chatgpt-affinity/driver/verify \
  "${AUTH[@]}")
printf '%s' "$VERIFY" | python3 -c '
import json,sys
x=json.load(sys.stdin)
assert x["authenticated"] is True, x
assert x["ready_match"] == "https://chatgpt.com/", x
'

THREAD=$(curl -fsS -X POST http://127.0.0.1:7331/v1/threads \
  "${AUTH[@]}" "${JSON[@]}" \
  -d '{"title":"ChatGPT affinity","model":"llmgateway-auto"}' \
  | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')

curl -fsS -D /tmp/chatgpt-affinity-1.headers -o /tmp/chatgpt-affinity-1.sse \
  -X POST "http://127.0.0.1:7331/v1/threads/$THREAD/messages" \
  "${AUTH[@]}" "${JSON[@]}" \
  -d '{"content":"chatgpt-one","stream":true}'
grep -qi '^x-llmgateway-route: chatgpt-affinity-route' /tmp/chatgpt-affinity-1.headers
grep -q 'data: \[DONE\]' /tmp/chatgpt-affinity-1.sse

curl -fsS -D /tmp/chatgpt-affinity-2.headers -o /tmp/chatgpt-affinity-2.sse \
  -X POST "http://127.0.0.1:7331/v1/threads/$THREAD/messages" \
  "${AUTH[@]}" "${JSON[@]}" \
  -d '{"content":"chatgpt-two","stream":true}'
grep -qi '^x-llmgateway-route: chatgpt-affinity-route' /tmp/chatgpt-affinity-2.headers
grep -q 'data: \[DONE\]' /tmp/chatgpt-affinity-2.sse

FIRST_TARGET=$(python3 - <<'PY'
import json
import os
with open(os.path.join(os.environ["PROFILE_DIR"], "browser-requests.jsonl"), encoding="utf-8") as f:
    requests = [json.loads(line) for line in f if line.strip()]
print(requests[-1]["target_id"])
PY
)

curl -fsS "http://127.0.0.1:$DEBUGGER_PORT/json/close/$FIRST_TARGET" >/dev/null

curl -fsS -D /tmp/chatgpt-affinity-3.headers -o /tmp/chatgpt-affinity-3.sse \
  -X POST "http://127.0.0.1:7331/v1/threads/$THREAD/messages" \
  "${AUTH[@]}" "${JSON[@]}" \
  -d '{"content":"chatgpt-three-after-close","stream":true}'
grep -qi '^x-llmgateway-route: chatgpt-affinity-route' /tmp/chatgpt-affinity-3.headers
grep -q 'data: \[DONE\]' /tmp/chatgpt-affinity-3.sse

THREAD_B=$(curl -fsS -X POST http://127.0.0.1:7331/v1/threads \
  "${AUTH[@]}" "${JSON[@]}" \
  -d '{"title":"ChatGPT affinity B","model":"llmgateway-auto"}' \
  | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')

curl -fsS -D /tmp/chatgpt-affinity-b1.headers -o /tmp/chatgpt-affinity-b1.sse \
  -X POST "http://127.0.0.1:7331/v1/threads/$THREAD_B/messages" \
  "${AUTH[@]}" "${JSON[@]}" \
  -d '{"content":"chatgpt-new-thread","stream":true}'
grep -qi '^x-llmgateway-route: chatgpt-affinity-route' /tmp/chatgpt-affinity-b1.headers
grep -q 'data: \[DONE\]' /tmp/chatgpt-affinity-b1.sse

python3 <<'PY'
import json
import os
import sqlite3

profile = os.environ["PROFILE_DIR"]

with open(os.path.join(profile, "opened-targets.log"), encoding="utf-8") as f:
    opened = [line.strip() for line in f if line.strip()]
assert opened == [
    "https://chatgpt.com/",
    "https://chatgpt.com/c/ci-thread-1",
    "https://chatgpt.com/",
], opened

with open(os.path.join(profile, "browser-requests.jsonl"), encoding="utf-8") as f:
    requests = [json.loads(line) for line in f if line.strip()]
recent = requests[-4:]
assert len(recent) == 4, recent
assert any(m.get("content") == "chatgpt-one" for m in recent[0]["messages"]), recent[0]
assert recent[1]["messages"] == [{"role": "user", "content": "chatgpt-two"}], recent[1]
assert recent[2]["messages"] == [{"role": "user", "content": "chatgpt-three-after-close"}], recent[2]
assert recent[3]["messages"] == [{"role": "user", "content": "chatgpt-new-thread"}], recent[3]
assert recent[0]["target_id"] == recent[1]["target_id"], recent
assert recent[2]["target_id"] != recent[1]["target_id"], recent
assert recent[3]["target_id"] != recent[2]["target_id"], recent

db = sqlite3.connect("data/llmgateway.db")
rows = db.execute(
    """SELECT provider, account_id, conversation_url, last_synced_ordinal
       FROM provider_conversations
       WHERE account_id = 'chatgpt-affinity'
       ORDER BY conversation_url"""
).fetchall()
assert len(rows) == 2, rows
assert all(row[0] == "chatgpt-web" for row in rows), rows
assert all(row[1] == "chatgpt-affinity" for row in rows), rows
urls = {row[2] for row in rows}
assert urls == {
    "https://chatgpt.com/c/ci-thread-1",
    "https://chatgpt.com/c/ci-thread-2",
}, rows
assert all(row[3] > 0 for row in rows), rows
db.close()
PY

echo "ChatGPT browser adapter + native affinity + close-tab recovery smoke passed"
