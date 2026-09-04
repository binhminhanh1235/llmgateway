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

python3 <<'PY'
import json
import os
import sqlite3

profile = os.environ["PROFILE_DIR"]

with open(os.path.join(profile, "opened-targets.log"), encoding="utf-8") as f:
    opened = [line.strip() for line in f if line.strip()]
assert opened == ["https://chatgpt.com/"], opened

with open(os.path.join(profile, "browser-requests.jsonl"), encoding="utf-8") as f:
    requests = [json.loads(line) for line in f if line.strip()]
recent = requests[-2:]
assert len(recent) == 2, recent
assert any(m.get("content") == "chatgpt-one" for m in recent[0]["messages"]), recent[0]
assert recent[1]["messages"] == [{"role": "user", "content": "chatgpt-two"}], recent[1]
assert recent[0]["target_id"] == recent[1]["target_id"], recent

db = sqlite3.connect("data/llmgateway.db")
rows = db.execute(
    """SELECT provider, account_id, conversation_url, last_synced_ordinal
       FROM provider_conversations
       WHERE account_id = 'chatgpt-affinity'"""
).fetchall()
assert len(rows) == 1, rows
provider, account, url, cursor = rows[0]
assert provider == "chatgpt-web", rows
assert account == "chatgpt-affinity", rows
assert url == "https://chatgpt.com/c/ci-thread-1", rows
assert cursor > 0, rows
db.close()
PY

echo "ChatGPT browser adapter + native conversation affinity smoke passed"
