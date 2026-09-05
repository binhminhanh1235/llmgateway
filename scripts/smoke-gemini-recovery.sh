#!/usr/bin/env bash
set -euo pipefail

export LLMGATEWAY_API_KEY="ci-gemini-recovery-key"
export FAKE_API_KEY="fake-key"
export LLMGATEWAY_CONFIG="/tmp/llmgateway-gemini-recovery.toml"
PROFILE_ROOT="/tmp/llmgateway-gemini-recovery-profiles"
FAKE_CHROMIUM="/tmp/llmgateway-fake-gemini-recovery-chromium"
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

python3 scripts/fake-openai.py >/tmp/llmgateway-gemini-recovery-api.log 2>&1 &
FAKE_PID=$!
cargo build --quiet
./target/debug/llmgateway >/tmp/llmgateway-gemini-recovery.log 2>&1 &
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

AUTH=(-H "Authorization: Bearer \${LLMGATEWAY_API_KEY}")
JSON=(-H "Content-Type: application/json")

curl -fsS -X POST http://127.0.0.1:7331/_llmgateway/browser-account-setup \
  "\${AUTH[@]}" "\${JSON[@]}" \
  -d '{"provider":"gemini","account_id":"gemini-recovery","label":"Gemini Recovery CI","priority":5}' \
  >/tmp/llmgateway-gemini-recovery-create.json

LAUNCH=$(curl -fsS -X POST \
  http://127.0.0.1:7331/_llmgateway/browser-sessions/gemini-recovery/driver/launch \
  "\${AUTH[@]}")
BROWSER_PID=$(printf '%s' "$LAUNCH" | python3 -c 'import json,sys; print(json.load(sys.stdin)["launch"]["pid"] or "")')
PROFILE_DIR=$(printf '%s' "$LAUNCH" | python3 -c 'import json,sys; print(json.load(sys.stdin)["launch"]["profile_dir"])')
export PROFILE_DIR

curl -fsS -X POST \
  http://127.0.0.1:7331/_llmgateway/browser-sessions/gemini-recovery/driver/verify \
  "\${AUTH[@]}" >/tmp/llmgateway-gemini-recovery-verify.json

THREAD_A=$(curl -fsS -X POST http://127.0.0.1:7331/v1/threads \
  "\${AUTH[@]}" "\${JSON[@]}" \
  -d '{"title":"Gemini recovery A","model":"llmgateway-auto"}' \
  | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')
export THREAD_A

send_stream() {
  local prompt="$1"
  local label="$2"
  curl -fsS -D "/tmp/gemini-recovery-\${label}.headers" \
    -o "/tmp/gemini-recovery-\${label}.sse" \
    -X POST "http://127.0.0.1:7331/v1/threads/$THREAD_A/messages" \
    "\${AUTH[@]}" "\${JSON[@]}" \
    -d "$(python3 - "$prompt" <<'PY'
import json,sys
print(json.dumps({"content":sys.argv[1],"stream":True}))
PY
)"
  grep -qi '^x-llmgateway-route: gemini-recovery-route' "/tmp/gemini-recovery-\${label}.headers"
  grep -Fq 'data: [DONE]' "/tmp/gemini-recovery-\${label}.sse"
  grep -Eq '"finish_reason"[[:space:]]*:[[:space:]]*"[^"]+"' "/tmp/gemini-recovery-\${label}.sse"
}

affinity_json() {
  local output="$1"
  curl -fsS \
    "http://127.0.0.1:7331/_llmgateway/threads/$THREAD_A/browser-affinity/gemini-recovery" \
    "\${AUTH[@]}" >"$output"
}

request_target() {
  local index="$1"
  python3 - "$PROFILE_DIR/browser-requests.jsonl" "$index" <<'PY'
import json,sys
rows=[json.loads(line) for line in open(sys.argv[1],encoding="utf-8") if line.strip()]
print(rows[int(sys.argv[2])]["target_id"])
PY
}

opened_count() {
  if [ -f "$PROFILE_DIR/opened-targets.log" ]; then
    grep -cve '^[[:space:]]*$' "$PROFILE_DIR/opened-targets.log" || true
  else
    echo 0
  fi
}

echo "[gemini-recovery] Scenario 1: establish persistent native conversation"
send_stream "recovery-a1" "a1"
affinity_json /tmp/gemini-recovery-affinity-a1.json
NATIVE_A=$(python3 -c 'import json; print(json.load(open("/tmp/gemini-recovery-affinity-a1.json"))["mapping"]["conversation_url"])')
ORD_A1=$(python3 -c 'import json; print(json.load(open("/tmp/gemini-recovery-affinity-a1.json"))["mapping"]["last_synced_ordinal"])')
TARGET_A1=$(request_target -1)

[ "$NATIVE_A" = "https://gemini.google.com/app/ci-thread-1" ]
[ "$ORD_A1" -gt 0 ]

echo "[gemini-recovery] Scenario 2: close mapped target and reopen native URL exactly once"
CDP_PORT=$(head -n 1 "$PROFILE_DIR/DevToolsActivePort")
OPEN_BEFORE_A2=$(opened_count)
curl -fsS "http://127.0.0.1:\${CDP_PORT}/json/close/\${TARGET_A1}" >/dev/null

python3 - "$CDP_PORT" "$TARGET_A1" <<'PY'
import json,sys,urllib.request
port,target=sys.argv[1:3]
targets=json.load(urllib.request.urlopen(f"http://127.0.0.1:{port}/json/list"))
assert all(item.get("id") != target for item in targets), targets
PY

send_stream "recovery-a2" "a2"
OPEN_AFTER_A2=$(opened_count)
[ "$OPEN_AFTER_A2" -eq $((OPEN_BEFORE_A2 + 1)) ]
[ "$(tail -n 1 "$PROFILE_DIR/opened-targets.log")" = "$NATIVE_A" ]

TARGET_A2=$(request_target -1)
[ "$TARGET_A2" != "$TARGET_A1" ]
grep -Fq "target=$TARGET_A2 url=$NATIVE_A" "$PROFILE_DIR/recovery-history-order.log"

python3 - "$PROFILE_DIR/browser-requests.jsonl" "$TARGET_A2" <<'PY'
import json,sys
rows=[json.loads(line) for line in open(sys.argv[1],encoding="utf-8") if line.strip()]
target=sys.argv[2]
recent=[row for row in rows if row.get("target_id") == target]
assert recent, rows
assert recent[-1]["messages"] == [{"role":"user","content":"recovery-a2"}], recent[-1]
PY

affinity_json /tmp/gemini-recovery-affinity-a2.json
python3 - "$NATIVE_A" "$ORD_A1" <<'PY'
import json,sys
affinity=json.load(open("/tmp/gemini-recovery-affinity-a2.json",encoding="utf-8"))
assert affinity["mapping"]["conversation_url"] == sys.argv[1], affinity
assert int(affinity["mapping"]["last_synced_ordinal"]) > int(sys.argv[2]), affinity
PY

echo "[gemini-recovery] Scenario 3: replacement target is reused for the next turn"
OPEN_BEFORE_A3=$(opened_count)
send_stream "recovery-a3" "a3"
OPEN_AFTER_A3=$(opened_count)
[ "$OPEN_AFTER_A3" -eq "$OPEN_BEFORE_A3" ]
TARGET_A3=$(request_target -1)
[ "$TARGET_A3" = "$TARGET_A2" ]

python3 - "$PROFILE_DIR/browser-requests.jsonl" "$TARGET_A3" <<'PY'
import json,sys
rows=[json.loads(line) for line in open(sys.argv[1],encoding="utf-8") if line.strip()]
target=sys.argv[2]
recent=[row for row in rows if row.get("target_id") == target]
assert recent[-1]["messages"] == [{"role":"user","content":"recovery-a3"}], recent[-1]
PY

echo "[gemini-recovery] Scenario 4: cancelled persistent stream clears target and native mapping"
curl -fsS "http://127.0.0.1:7331/v1/threads/$THREAD_A" "\${AUTH[@]}" >/tmp/gemini-recovery-thread-before-cancel.json
ASSISTANTS_BEFORE=$(python3 -c 'import json; x=json.load(open("/tmp/gemini-recovery-thread-before-cancel.json")); print(sum(1 for m in x["messages"] if m["role"]=="assistant"))')

python3 <<'PY'
import http.client,json,os
thread=os.environ["THREAD_A"]
key=os.environ["LLMGATEWAY_API_KEY"]
body=json.dumps({"content":"recovery-cancel","stream":True})
conn=http.client.HTTPConnection("127.0.0.1",7331,timeout=10)
conn.request(
    "POST",
    f"/v1/threads/{thread}/messages",
    body=body,
    headers={
        "Authorization":f"Bearer {key}",
        "Content-Type":"application/json",
        "Content-Length":str(len(body.encode())),
    },
)
response=conn.getresponse()
assert response.status == 200, (response.status,response.read())
assert response.getheader("x-llmgateway-route") == "gemini-recovery-route", dict(response.getheaders())
saw_partial=False
while True:
    line=response.readline()
    if not line:
        break
    if not line.startswith(b"data: "):
        continue
    data=line[6:].strip()
    if not data or data == b"[DONE]":
        continue
    event=json.loads(data)
    delta=((event.get("choices") or [{}])[0].get("delta") or {}).get("content")
    if delta:
        saw_partial=True
        break
assert saw_partial, "fake Gemini stream produced no partial assistant content"
conn.close()
PY

CANCEL_TARGET=$(request_target -1)
[ "$CANCEL_TARGET" = "$TARGET_A2" ]

for _ in {1..50}; do
  STARTED=$(cat "$PROFILE_DIR/stream-started" 2>/dev/null || true)
  CANCELLED=$(cat "$PROFILE_DIR/stream-cancelled" 2>/dev/null || true)
  CLOSED=$(cat "$PROFILE_DIR/target-closed" 2>/dev/null || true)
  MAPPING_COUNT=$(python3 - "$THREAD_A" <<'PY'
import sqlite3,sys
db=sqlite3.connect("data/llmgateway.db")
count=db.execute(
    "SELECT COUNT(*) FROM provider_conversations WHERE thread_id=? AND account_id='gemini-recovery'",
    (sys.argv[1],),
).fetchone()[0]
db.close()
print(count)
PY
)
  if [ -n "$STARTED" ] && [ "$CANCELLED" = "$STARTED" ] && [ "$CLOSED" = "$CANCEL_TARGET" ] && [ "$MAPPING_COUNT" -eq 0 ]; then
    break
  fi
  sleep 0.1
done

STARTED=$(cat "$PROFILE_DIR/stream-started")
CANCELLED=$(cat "$PROFILE_DIR/stream-cancelled")
CLOSED=$(cat "$PROFILE_DIR/target-closed")
[ "$CANCELLED" = "$STARTED" ]
[ "$CLOSED" = "$CANCEL_TARGET" ]
[ "$MAPPING_COUNT" -eq 0 ]

python3 - "$CDP_PORT" "$CANCEL_TARGET" <<'PY'
import json,sys,urllib.request
port,target=sys.argv[1:3]
targets=json.load(urllib.request.urlopen(f"http://127.0.0.1:{port}/json/list"))
assert all(item.get("id") != target for item in targets), targets
PY

curl -fsS "http://127.0.0.1:7331/v1/threads/$THREAD_A" "\${AUTH[@]}" >/tmp/gemini-recovery-thread-after-cancel.json
python3 - "$ASSISTANTS_BEFORE" <<'PY'
import json,sys
thread=json.load(open("/tmp/gemini-recovery-thread-after-cancel.json",encoding="utf-8"))
assistants=[m for m in thread["messages"] if m["role"]=="assistant"]
assert len(assistants) == int(sys.argv[1]), assistants
assert all(str(m.get("content") or "").strip() for m in assistants), assistants
PY

echo "[gemini-recovery] Scenario 5: next turn starts from safe fresh native state"
OPEN_BEFORE_A4=$(opened_count)
send_stream "recovery-a4" "a4"
OPEN_AFTER_A4=$(opened_count)
[ "$OPEN_AFTER_A4" -eq $((OPEN_BEFORE_A4 + 1)) ]
[ "$(tail -n 1 "$PROFILE_DIR/opened-targets.log")" = "https://gemini.google.com/app" ]
TARGET_A4=$(request_target -1)
[ "$TARGET_A4" != "$CANCEL_TARGET" ]

affinity_json /tmp/gemini-recovery-affinity-a4.json
python3 - "$NATIVE_A" <<'PY'
import json,sys
affinity=json.load(open("/tmp/gemini-recovery-affinity-a4.json",encoding="utf-8"))
mapping=affinity["mapping"]
assert mapping is not None, affinity
assert mapping["conversation_url"] != sys.argv[1], affinity
assert mapping["conversation_url"] == "https://gemini.google.com/app/ci-thread-2", affinity
assert int(mapping["last_synced_ordinal"]) > 0, affinity
PY

curl -fsS -X POST \
  http://127.0.0.1:7331/_llmgateway/browser-sessions/gemini-recovery/driver/stop \
  "\${AUTH[@]}" >/dev/null
BROWSER_PID=""

echo "Gemini close-tab + cancelled-stream recovery E2E smoke passed"
