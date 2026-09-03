#!/usr/bin/env bash
set -euo pipefail

export LLMGATEWAY_API_KEY="ci-local-key"
export FAKE_API_KEY="fake-key"
export LLMGATEWAY_CONFIG="/tmp/llmgateway-smoke.toml"

rm -f data/llmgateway.db data/llmgateway.db-shm data/llmgateway.db-wal
mkdir -p data

cat >/tmp/llmgateway-smoke.toml <<'EOF'
[server]
host = "127.0.0.1"
port = 7331

[api]
key_env = "LLMGATEWAY_API_KEY"
default_model = "llmgateway-auto"

[storage]
database_url = "sqlite://data/llmgateway.db"

[context]
enabled = true
target_tokens = 1024
reserve_output_tokens = 128
recent_messages = 2
compaction_trigger_ratio = 0.5
summary_input_tokens = 512
summary_max_tokens = 128
summary_model = "llmgateway-auto"

[[providers]]
id = "fake"
kind = "openai-compatible"
base_url = "http://127.0.0.1:18080/v1"
models_path = "models"

[[accounts]]
id = "fake-primary"
provider = "fake"
api_key_env = "FAKE_API_KEY"
auth_style = "bearer"
enabled = true
discover_models = false

[[routes]]
id = "fake-route"
account = "fake-primary"
model = "fake-model"
priority = 10
enabled = true
capabilities = ["chat", "tools", "coding"]

[virtual_models.llmgateway-auto]
routes = ["fake-route"]

[virtual_models.llmgateway-coding]
routes = ["fake-route"]

[virtual_models.llmgateway-best]
routes = ["fake-route"]
EOF

python3 scripts/fake-openai.py >/tmp/llmgateway-fake.log 2>&1 &
FAKE_PID=$!
cargo build --quiet
./target/debug/llmgateway >/tmp/llmgateway-smoke.log 2>&1 &
PID=$!
trap 'kill "$PID" "$FAKE_PID" 2>/dev/null || true; rm -f data/llmgateway.db data/llmgateway.db-shm data/llmgateway.db-wal /tmp/llmgateway-smoke.toml' EXIT

for _ in {1..40}; do
  if curl -fsS http://127.0.0.1:7331/_llmgateway/health >/dev/null; then
    break
  fi
  sleep 0.2
done

curl -fsS http://127.0.0.1:7331/ | grep -q "llmgateway"
curl -fsS http://127.0.0.1:7331/ui/app.js | grep -q "llmgateway.threads.v1"
curl -fsS http://127.0.0.1:7331/v1/models \
  -H "Authorization: Bearer ${LLMGATEWAY_API_KEY}" | grep -q "llmgateway-auto"

THREAD_JSON=$(curl -fsS -X POST \
  http://127.0.0.1:7331/v1/threads \
  -H "Authorization: Bearer ${LLMGATEWAY_API_KEY}" \
  -H "Content-Type: application/json" \
  -d '{"title":"CI thread","model":"llmgateway-auto"}')
THREAD_ID=$(printf '%s' "$THREAD_JSON" | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')

THREAD_STREAM=$(curl -fsS -N -X POST \
  "http://127.0.0.1:7331/v1/threads/${THREAD_ID}/messages" \
  -H "Authorization: Bearer ${LLMGATEWAY_API_KEY}" \
  -H "Content-Type: application/json" \
  -d '{"content":"hello thread","stream":true}')
printf '%s' "$THREAD_STREAM" | grep -q 'fake reply messages=1'
sleep 0.1

SECOND_TURN=$(curl -fsS -X POST \
  "http://127.0.0.1:7331/v1/threads/${THREAD_ID}/messages" \
  -H "Authorization: Bearer ${LLMGATEWAY_API_KEY}" \
  -H "Content-Type: application/json" \
  -d '{"content":"second turn","stream":false}')
printf '%s' "$SECOND_TURN" | grep -q 'fake reply messages=3'

THREAD_DETAIL=$(curl -fsS "http://127.0.0.1:7331/v1/threads/${THREAD_ID}" \
  -H "Authorization: Bearer ${LLMGATEWAY_API_KEY}")
printf '%s' "$THREAD_DETAIL" | python3 -c '
import json,sys
x=json.load(sys.stdin)
assert len(x["messages"]) == 4, x
assert x["sticky_route"] == "fake-route", x
assert x["messages"][1]["message"]["content"] == "fake reply messages=1", x
assert x["messages"][3]["message"]["content"] == "fake reply messages=3", x
'

CONTEXT_BEFORE=$(curl -fsS "http://127.0.0.1:7331/v1/threads/${THREAD_ID}/context" \
  -H "Authorization: Bearer ${LLMGATEWAY_API_KEY}")
printf '%s' "$CONTEXT_BEFORE" | grep -q '"state":"full"'

COMPACTED=$(curl -fsS -X POST \
  "http://127.0.0.1:7331/v1/threads/${THREAD_ID}/compact" \
  -H "Authorization: Bearer ${LLMGATEWAY_API_KEY}" \
  -H "Content-Type: application/json" \
  -d '{}')
printf '%s' "$COMPACTED" | python3 -c '
import json,sys
x=json.load(sys.stdin)
assert x["state"] == "compressed", x
assert x["checkpoint"] is not None, x
assert x["checkpoint"]["through_ordinal"] == 2, x
assert x["checkpoint"]["summary"] == "fake reply messages=2", x
assert x["checkpoint"]["route_id"] == "fake-route", x
'

AFTER_COMPACT=$(curl -fsS -X POST \
  "http://127.0.0.1:7331/v1/threads/${THREAD_ID}/messages" \
  -H "Authorization: Bearer ${LLMGATEWAY_API_KEY}" \
  -H "Content-Type: application/json" \
  -d '{"content":"after compact","stream":false}')
printf '%s' "$AFTER_COMPACT" | grep -q 'fake reply messages=4'

THREAD_AFTER=$(curl -fsS "http://127.0.0.1:7331/v1/threads/${THREAD_ID}" \
  -H "Authorization: Bearer ${LLMGATEWAY_API_KEY}")
printf '%s' "$THREAD_AFTER" | python3 -c '
import json,sys
x=json.load(sys.stdin)
assert len(x["messages"]) == 6, x
assert x["messages"][4]["message"]["content"] == "after compact", x
assert x["messages"][5]["message"]["content"] == "fake reply messages=4", x
'

CONTEXT_AFTER=$(curl -fsS "http://127.0.0.1:7331/v1/threads/${THREAD_ID}/context" \
  -H "Authorization: Bearer ${LLMGATEWAY_API_KEY}")
printf '%s' "$CONTEXT_AFTER" | python3 -c '
import json,sys
x=json.load(sys.stdin)
assert x["state"] == "compressed", x
assert x["checkpoint"]["through_ordinal"] == 2, x
assert x["prepared_tokens"] <= x["source_tokens"], x
'

FIRST_RESPONSE=$(curl -fsS -X POST http://127.0.0.1:7331/v1/responses \
  -H "Authorization: Bearer ${LLMGATEWAY_API_KEY}" \
  -H "Content-Type: application/json" \
  -d '{"model":"llmgateway-coding","input":"first"}')
RESPONSE_ID=$(printf '%s' "$FIRST_RESPONSE" | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')
printf '%s' "$FIRST_RESPONSE" | grep -q 'fake reply messages=1'

SECOND_RESPONSE=$(curl -fsS -X POST http://127.0.0.1:7331/v1/responses \
  -H "Authorization: Bearer ${LLMGATEWAY_API_KEY}" \
  -H "Content-Type: application/json" \
  -d "{\"model\":\"llmgateway-coding\",\"previous_response_id\":\"${RESPONSE_ID}\",\"input\":\"second\"}")
printf '%s' "$SECOND_RESPONSE" | grep -q 'fake reply messages=3'

curl -fsS -X PATCH \
  http://127.0.0.1:7331/_llmgateway/accounts/fake-primary/models \
  -H "Authorization: Bearer ${LLMGATEWAY_API_KEY}" \
  -H "Content-Type: application/json" \
  -d '{"model_id":"fake/fake-model","enabled":false}' | grep -q '"enabled":false'

curl -fsS -X DELETE "http://127.0.0.1:7331/v1/threads/${THREAD_ID}" \
  -H "Authorization: Bearer ${LLMGATEWAY_API_KEY}" | grep -q '"deleted":true'

echo "llmgateway local smoke test passed"
