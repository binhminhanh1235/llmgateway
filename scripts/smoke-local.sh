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
retrieval_enabled = true
retrieval_max_chunks = 2
retrieval_max_tokens = 320
retrieval_min_score = 0.2

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

ERROR_HEADERS=/tmp/llmgateway-error-contract.headers
ERROR_BODY=/tmp/llmgateway-error-contract.json

assert_json_error() {
  local expected_status="$1"
  local expected_type="$2"
  local actual_status="$3"
  test "$actual_status" = "$expected_status"
  grep -qi '^content-type: application/json' "$ERROR_HEADERS"
  python3 - "$ERROR_BODY" "$expected_type" <<'PY'
import json,sys
with open(sys.argv[1], encoding="utf-8") as f:
    payload=json.load(f)
assert payload["error"]["type"] == sys.argv[2], payload
assert isinstance(payload["error"]["message"], str) and payload["error"]["message"], payload
PY
}

STATUS=$(curl -sS -D "$ERROR_HEADERS" -o "$ERROR_BODY" -w '%{http_code}' -X POST \
  http://127.0.0.1:7331/v1/threads/nonexistent-thread-id/retrieve \
  -H "Authorization: Bearer ${LLMGATEWAY_API_KEY}" \
  -H "Content-Type: application/json" \
  -d '{"query":"test"}')
assert_json_error 404 not_found_error "$STATUS"

for endpoint in \
  "/_llmgateway/accounts/nonexistent-account-xyz/usage" \
  "/_llmgateway/accounts/nonexistent-account-xyz/models"; do
  STATUS=$(curl -sS -D "$ERROR_HEADERS" -o "$ERROR_BODY" -w '%{http_code}' \
    "http://127.0.0.1:7331${endpoint}" \
    -H "Authorization: Bearer ${LLMGATEWAY_API_KEY}")
  assert_json_error 404 not_found_error "$STATUS"
done

STATUS=$(curl -sS -D "$ERROR_HEADERS" -o "$ERROR_BODY" -w '%{http_code}' -X POST \
  http://127.0.0.1:7331/_llmgateway/accounts/nonexistent-account-xyz/quota/reset \
  -H "Authorization: Bearer ${LLMGATEWAY_API_KEY}")
assert_json_error 404 not_found_error "$STATUS"

STATUS=$(curl -sS -D "$ERROR_HEADERS" -o "$ERROR_BODY" -w '%{http_code}' -X POST \
  http://127.0.0.1:7331/_llmgateway/accounts/nonexistent-account-xyz/models/refresh \
  -H "Authorization: Bearer ${LLMGATEWAY_API_KEY}")
assert_json_error 404 not_found_error "$STATUS"

STATUS=$(curl -sS -D "$ERROR_HEADERS" -o "$ERROR_BODY" -w '%{http_code}' -X PATCH \
  http://127.0.0.1:7331/_llmgateway/accounts/nonexistent-account-xyz/models \
  -H "Authorization: Bearer ${LLMGATEWAY_API_KEY}" \
  -H "Content-Type: application/json" \
  -d '{"model_id":"fake/fake-model","enabled":false}')
assert_json_error 404 not_found_error "$STATUS"

STATUS=$(curl -sS -D "$ERROR_HEADERS" -o "$ERROR_BODY" -w '%{http_code}' -X POST \
  http://127.0.0.1:7331/v1/chat/completions \
  -H "Authorization: Bearer ${LLMGATEWAY_API_KEY}" \
  -H "Content-Type: application/json" \
  -d '{malformed-json}')
assert_json_error 400 invalid_request_error "$STATUS"
python3 - "$ERROR_BODY" <<'PY'
import json,sys
with open(sys.argv[1], encoding="utf-8") as f:
    payload=json.load(f)
assert payload["error"]["message"].startswith("Failed to parse request JSON:"), payload
PY

STATUS=$(curl -sS -D "$ERROR_HEADERS" -o "$ERROR_BODY" -w '%{http_code}' -X POST \
  http://127.0.0.1:7331/_llmgateway/browser-sessions/nonexistent-session/attention \
  -H "Authorization: Bearer ${LLMGATEWAY_API_KEY}" \
  -H "Content-Type: application/json" \
  -d '{}')
assert_json_error 422 invalid_request_error "$STATUS"

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
  -d '{"content":"invoice optimistic locking conflicts return HTTP 409","stream":true}')
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
assert x["messages"][0]["message"]["content"] == "invoice optimistic locking conflicts return HTTP 409", x
assert x["messages"][1]["message"]["content"] == "fake reply messages=1", x
assert x["messages"][3]["message"]["content"] == "fake reply messages=3", x
'

CONTEXT_BEFORE=$(curl -fsS "http://127.0.0.1:7331/v1/threads/${THREAD_ID}/context" \
  -H "Authorization: Bearer ${LLMGATEWAY_API_KEY}")
printf '%s' "$CONTEXT_BEFORE" | python3 -c '
import json,sys
x=json.load(sys.stdin)
assert x["state"] == "full", x
assert x["memory"] is None, x
'

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
assert "Structured conversation memory (schema v1)" in x["checkpoint"]["summary"], x
assert x["checkpoint"]["route_id"] == "fake-route", x
memory=x["memory"]
assert memory is not None, x
assert memory["through_ordinal"] == 2, memory
assert memory["schema_version"] == 1, memory
assert memory["route_id"] == "fake-route", memory
assert memory["memory"]["facts"] == ["The smoke thread has durable context"], memory
assert memory["memory"]["decisions"] == ["Use structured memory snapshots"], memory
assert memory["memory"]["constraints"] == ["Keep the full transcript in SQLite"], memory
assert memory["memory"]["code_context"] == ["thread_memories stores schema-versioned JSON"], memory
assert memory["memory"]["rolling_summary"].startswith("The thread is validating"), memory
'

MEMORY_VIEW=$(curl -fsS "http://127.0.0.1:7331/v1/threads/${THREAD_ID}/memory" \
  -H "Authorization: Bearer ${LLMGATEWAY_API_KEY}")
printf '%s' "$MEMORY_VIEW" | python3 -c '
import json,sys
x=json.load(sys.stdin)
assert x["memory"]["schema_version"] == 1, x
assert x["memory"]["through_ordinal"] == 2, x
assert x["memory"]["memory"]["user_preferences"] == ["Prefer local-first behavior"], x
'

AFTER_COMPACT=$(curl -fsS -X POST \
  "http://127.0.0.1:7331/v1/threads/${THREAD_ID}/messages" \
  -H "Authorization: Bearer ${LLMGATEWAY_API_KEY}" \
  -H "Content-Type: application/json" \
  -d '{"content":"after compact","stream":false}')
printf '%s' "$AFTER_COMPACT" | grep -q 'fake reply messages=4'
if printf '%s' "$AFTER_COMPACT" | grep -q 'retrieval=yes'; then
  echo "unexpected retrieval for unrelated query" >&2
  exit 1
fi

THREAD_AFTER=$(curl -fsS "http://127.0.0.1:7331/v1/threads/${THREAD_ID}" \
  -H "Authorization: Bearer ${LLMGATEWAY_API_KEY}")
printf '%s' "$THREAD_AFTER" | python3 -c '
import json,sys
x=json.load(sys.stdin)
assert len(x["messages"]) == 6, x
assert x["messages"][4]["message"]["content"] == "after compact", x
assert x["messages"][5]["message"]["content"] == "fake reply messages=4", x
'

SECOND_COMPACTED=$(curl -fsS -X POST \
  "http://127.0.0.1:7331/v1/threads/${THREAD_ID}/compact" \
  -H "Authorization: Bearer ${LLMGATEWAY_API_KEY}" \
  -H "Content-Type: application/json" \
  -d '{}')
printf '%s' "$SECOND_COMPACTED" | python3 -c '
import json,sys
x=json.load(sys.stdin)
assert x["checkpoint"]["through_ordinal"] == 4, x
assert x["memory"]["through_ordinal"] == 4, x
assert x["memory"]["schema_version"] == 1, x
assert x["memory"]["memory"]["facts"] == ["The smoke thread has durable context"], x
assert x["memory"]["memory"]["open_questions"] == ["What should v0.7 optimize next?"], x
'

RETRIEVAL_HEADERS=$(mktemp)
AFTER_SECOND_COMPACT=$(curl -fsS -D "$RETRIEVAL_HEADERS" -X POST \
  "http://127.0.0.1:7331/v1/threads/${THREAD_ID}/messages" \
  -H "Authorization: Bearer ${LLMGATEWAY_API_KEY}" \
  -H "Content-Type: application/json" \
  -d '{"content":"remind me how invoice optimistic locking HTTP 409 conflicts work","stream":false}')
printf '%s' "$AFTER_SECOND_COMPACT" | grep -q 'fake reply messages=4 retrieval=yes'
grep -qi '^x-llmgateway-retrieved-chunks: [1-9]' "$RETRIEVAL_HEADERS"
rm -f "$RETRIEVAL_HEADERS"

THREAD_FINAL=$(curl -fsS "http://127.0.0.1:7331/v1/threads/${THREAD_ID}" \
  -H "Authorization: Bearer ${LLMGATEWAY_API_KEY}")
printf '%s' "$THREAD_FINAL" | python3 -c '
import json,sys
x=json.load(sys.stdin)
assert len(x["messages"]) == 8, x
assert x["messages"][0]["message"]["content"] == "invoice optimistic locking conflicts return HTTP 409", x
assert x["messages"][4]["message"]["content"] == "after compact", x
assert "invoice optimistic locking HTTP 409" in x["messages"][6]["message"]["content"], x
assert x["messages"][7]["message"]["content"].endswith("retrieval=yes"), x
'

CONTEXT_AFTER=$(curl -fsS "http://127.0.0.1:7331/v1/threads/${THREAD_ID}/context" \
  -H "Authorization: Bearer ${LLMGATEWAY_API_KEY}")
printf '%s' "$CONTEXT_AFTER" | python3 -c '
import json,sys
x=json.load(sys.stdin)
assert x["state"] == "compressed", x
assert x["checkpoint"]["through_ordinal"] == 4, x
assert x["memory"]["through_ordinal"] == 4, x
assert x["memory"]["schema_version"] == 1, x
assert x["memory"]["memory"]["entities"] == ["llmgateway", "ContextEngine"], x
assert x["prepared_tokens"] <= x["budget_tokens"], x
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
