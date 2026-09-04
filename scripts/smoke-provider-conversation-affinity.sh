#!/usr/bin/env bash
set -euo pipefail

export LLMGATEWAY_API_KEY="ci-provider-conversation-key"
export FAKE_API_KEY="fake-key"
export LLMGATEWAY_CONFIG="/tmp/llmgateway-provider-conversation-affinity.toml"
PROFILE_ROOT="/tmp/llmgateway-provider-conversation-affinity-profiles"
FAKE_CHROMIUM="/tmp/llmgateway-fake-affinity-chromium"
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

python3 scripts/fake-openai.py >/tmp/llmgateway-provider-conversation-api.log 2>&1 &
FAKE_PID=$!
cargo build --quiet
./target/debug/llmgateway >/tmp/llmgateway-provider-conversation.log 2>&1 &
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

curl -fsS -X POST http://127.0.0.1:7331/_llmgateway/browser-account-setup   "${AUTH[@]}" "${JSON[@]}"   -d '{"provider":"gemini","account_id":"gemini-affinity","label":"Gemini Affinity CI","priority":5}'   >/tmp/llmgateway-provider-conversation-create.json

LAUNCH=$(curl -fsS -X POST   http://127.0.0.1:7331/_llmgateway/browser-sessions/gemini-affinity/driver/launch   "${AUTH[@]}")
BROWSER_PID=$(printf '%s' "$LAUNCH" | python3 -c 'import json,sys; print(json.load(sys.stdin)["launch"]["pid"] or "")')
PROFILE_DIR=$(printf '%s' "$LAUNCH" | python3 -c 'import json,sys; print(json.load(sys.stdin)["launch"]["profile_dir"])')
export PROFILE_DIR

curl -fsS -X POST   http://127.0.0.1:7331/_llmgateway/browser-sessions/gemini-affinity/driver/verify   "${AUTH[@]}" >/tmp/llmgateway-provider-conversation-verify.json

THREAD_A=$(curl -fsS -X POST http://127.0.0.1:7331/v1/threads   "${AUTH[@]}" "${JSON[@]}"   -d '{"title":"Affinity A","model":"llmgateway-auto"}'   | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')

curl -fsS -D /tmp/affinity-a1.headers -o /tmp/affinity-a1.json   -X POST "http://127.0.0.1:7331/v1/threads/$THREAD_A/messages"   "${AUTH[@]}" "${JSON[@]}"   -d '{"content":"alpha-one","stream":true}'
grep -qi '^x-llmgateway-route: gemini-affinity-route' /tmp/affinity-a1.headers

curl -fsS -D /tmp/affinity-a2.headers -o /tmp/affinity-a2.json   -X POST "http://127.0.0.1:7331/v1/threads/$THREAD_A/messages"   "${AUTH[@]}" "${JSON[@]}"   -d '{"content":"alpha-two","stream":true}'
grep -qi '^x-llmgateway-route: gemini-affinity-route' /tmp/affinity-a2.headers

THREAD_B=$(curl -fsS -X POST http://127.0.0.1:7331/v1/threads   "${AUTH[@]}" "${JSON[@]}"   -d '{"title":"Affinity B","model":"llmgateway-auto"}'   | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')

curl -fsS -D /tmp/affinity-b1.headers -o /tmp/affinity-b1.json   -X POST "http://127.0.0.1:7331/v1/threads/$THREAD_B/messages"   "${AUTH[@]}" "${JSON[@]}"   -d '{"content":"beta-one","stream":true}'
grep -qi '^x-llmgateway-route: gemini-affinity-route' /tmp/affinity-b1.headers

python3 <<'PY'
import json
import os
import sqlite3

profile = os.environ["PROFILE_DIR"]

with open(os.path.join(profile, "opened-targets.log"), encoding="utf-8") as f:
    opened = [line.strip() for line in f if line.strip()]
assert opened[:2] == [
    "https://gemini.google.com/app",
    "https://gemini.google.com/app",
], opened
assert len(opened) == 2, opened

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

db = sqlite3.connect("data/llmgateway.db")
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

echo "provider conversation affinity smoke passed"
