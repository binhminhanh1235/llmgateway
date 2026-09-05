#!/usr/bin/env bash
set -euo pipefail

export LLMGATEWAY_API_KEY="ci-local-key"
export LLMGATEWAY_FILE_CLIENT_A_KEY="ci-file-client-a"
export LLMGATEWAY_FILE_CLIENT_B_KEY="ci-file-client-b"
export FAKE_API_KEY="fake-key"
export LLMGATEWAY_CONFIG="/tmp/llmgateway-smoke.toml"

rm -f data/llmgateway.db data/llmgateway.db-shm data/llmgateway.db-wal
rm -rf data/artifacts
mkdir -p data

cat >/tmp/llmgateway-smoke.toml <<'EOF'
[server]
host = "127.0.0.1"
port = 7331

[api]
key_env = "LLMGATEWAY_API_KEY"
default_model = "llmgateway-auto"

[clients.file-a]
key_env = "LLMGATEWAY_FILE_CLIENT_A_KEY"
enabled = true

[clients.file-b]
key_env = "LLMGATEWAY_FILE_CLIENT_B_KEY"
enabled = true

[storage]
database_url = "sqlite://data/llmgateway.db"

[artifacts]
root = "data/artifacts"
max_file_size_bytes = 1024
max_request_size_bytes = 2048
max_files_per_request = 2
remote_url_ingestion = false

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
trap 'kill "$PID" "$FAKE_PID" 2>/dev/null || true; rm -f data/llmgateway.db data/llmgateway.db-shm data/llmgateway.db-wal /tmp/llmgateway-smoke.toml /tmp/llmgateway-artifact-a.txt /tmp/llmgateway-artifact-b.txt /tmp/llmgateway-artifact-spoof.pdf /tmp/llmgateway-artifact-big.bin /tmp/llmgateway-artifact-download.txt; rm -rf data/artifacts' EXIT

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
  http://127.0.0.1:7331/v1/chat/completions \
  -H "Authorization: Bearer ${LLMGATEWAY_API_KEY}" \
  -H "Content-Type: application/json" \
  -d '{"model":"llmgateway-auto","messages":[{"role":"user","content":[{"type":"image_url","image_url":{"url":"data:image/png;base64,AA=="}}]}]}' )
assert_json_error 400 unsupported_capability "$STATUS"

STATUS=$(curl -sS -D "$ERROR_HEADERS" -o "$ERROR_BODY" -w '%{http_code}' -X POST \
  http://127.0.0.1:7331/_llmgateway/browser-sessions/nonexistent-session/attention \
  -H "Authorization: Bearer ${LLMGATEWAY_API_KEY}" \
  -H "Content-Type: application/json" \
  -d '{}')
assert_json_error 422 invalid_request_error "$STATUS"

curl -fsS http://127.0.0.1:7331/ | grep -q "llmgateway"
curl -fsS http://127.0.0.1:7331/ui/app.js | grep -q "llmgateway.threads.v1"
MODELS_JSON=$(curl -fsS http://127.0.0.1:7331/v1/models \
  -H "Authorization: Bearer ${LLMGATEWAY_API_KEY}")
printf '%s' "$MODELS_JSON" | grep -q "llmgateway-auto"
printf '%s' "$MODELS_JSON" | python3 -c '
import json,sys
x=json.load(sys.stdin)
physical=next(item for item in x["data"] if item["id"]=="fake/fake-model")
legacy=physical["llmgateway"]["capabilities"]
structured=physical["llmgateway"]["multimodal_capabilities"]
assert "chat" in legacy, physical
assert structured["input_modalities"] == ["text"], structured
assert structured["output_modalities"] == ["text"], structured
'

CAPABILITIES_JSON=$(curl -fsS http://127.0.0.1:7331/v1/capabilities \
  -H "Authorization: Bearer ${LLMGATEWAY_API_KEY}")
printf '%s' "$CAPABILITIES_JSON" | python3 -c '
import json,sys
x=json.load(sys.stdin)
assert x["object"] == "llmgateway.capabilities", x
assert x["schema_version"] == 1, x
assert x["canonical_modalities"]["input"] == ["text","image","file","audio"], x
assert x["canonical_modalities"]["output"] == ["text","image","audio","file"], x
assert x["gateway_execution"]["input_modalities"] == ["text"], x
assert x["live_attachments"] is False, x
assert x["artifact_store"]["enabled"] is True, x
assert x["artifact_store"]["max_file_size_bytes"] == 1024, x
assert x["artifact_store"]["max_request_size_bytes"] == 2048, x
assert x["artifact_store"]["max_files_per_request"] == 2, x
assert x["artifact_store"]["remote_url_ingestion"] is False, x
assert any(a["id"]=="fake" and a["transport"]=="api" for a in x["adapters"]), x
'

printf 'artifact smoke payload\n' >/tmp/llmgateway-artifact-a.txt
cp /tmp/llmgateway-artifact-a.txt /tmp/llmgateway-artifact-b.txt

STATUS=$(curl -sS -D "$ERROR_HEADERS" -o "$ERROR_BODY" -w '%{http_code}' -X POST \
  http://127.0.0.1:7331/v1/files \
  -F 'purpose=assistants' \
  -F 'file=@/tmp/llmgateway-artifact-a.txt;type=text/plain')
assert_json_error 401 authentication_error "$STATUS"

FILE_A_JSON=$(curl -fsS -X POST http://127.0.0.1:7331/v1/files \
  -H "Authorization: Bearer ${LLMGATEWAY_FILE_CLIENT_A_KEY}" \
  -F 'purpose=assistants' \
  -F 'file=@/tmp/llmgateway-artifact-a.txt;type=text/plain')
FILE_A_ID=$(printf '%s' "$FILE_A_JSON" | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')
FILE_A_SHA=$(printf '%s' "$FILE_A_JSON" | python3 -c 'import json,sys; print(json.load(sys.stdin)["sha256"])')
printf '%s' "$FILE_A_JSON" | python3 -c '
import json,sys
x=json.load(sys.stdin)
assert x["object"] == "file", x
assert x["status"] == "processed", x
assert x["mime_type"] == "text/plain", x
assert x["bytes"] > 0, x
assert x["llmgateway"]["source"] == "api_upload", x
assert "path" not in x and "relative_path" not in x, x
'

FILE_A_META=$(curl -fsS "http://127.0.0.1:7331/v1/files/${FILE_A_ID}" \
  -H "Authorization: Bearer ${LLMGATEWAY_FILE_CLIENT_A_KEY}")
test "$(printf '%s' "$FILE_A_META" | python3 -c 'import json,sys; print(json.load(sys.stdin)["sha256"])')" = "$FILE_A_SHA"

curl -fsS "http://127.0.0.1:7331/v1/files/${FILE_A_ID}/content" \
  -H "Authorization: Bearer ${LLMGATEWAY_FILE_CLIENT_A_KEY}" \
  -o /tmp/llmgateway-artifact-download.txt
cmp /tmp/llmgateway-artifact-a.txt /tmp/llmgateway-artifact-download.txt
rm -f /tmp/llmgateway-artifact-download.txt

STATUS=$(curl -sS -D "$ERROR_HEADERS" -o "$ERROR_BODY" -w '%{http_code}' \
  "http://127.0.0.1:7331/v1/files/${FILE_A_ID}" \
  -H "Authorization: Bearer ${LLMGATEWAY_FILE_CLIENT_B_KEY}")
assert_json_error 404 not_found_error "$STATUS"

FILE_B_JSON=$(curl -fsS -X POST http://127.0.0.1:7331/v1/files \
  -H "Authorization: Bearer ${LLMGATEWAY_FILE_CLIENT_A_KEY}" \
  -F 'purpose=assistants' \
  -F 'file=@/tmp/llmgateway-artifact-b.txt;type=text/plain')
FILE_B_ID=$(printf '%s' "$FILE_B_JSON" | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')
FILE_B_SHA=$(printf '%s' "$FILE_B_JSON" | python3 -c 'import json,sys; print(json.load(sys.stdin)["sha256"])')
test "$FILE_A_ID" != "$FILE_B_ID"
test "$FILE_A_SHA" = "$FILE_B_SHA"
test "$(find data/artifacts/blobs -type f | wc -l | tr -d ' ')" = "1"

printf '%%PDF-1.7\nspoof\n' >/tmp/llmgateway-artifact-spoof.pdf
STATUS=$(curl -sS -D "$ERROR_HEADERS" -o "$ERROR_BODY" -w '%{http_code}' -X POST \
  http://127.0.0.1:7331/v1/files \
  -H "Authorization: Bearer ${LLMGATEWAY_FILE_CLIENT_A_KEY}" \
  -F 'purpose=assistants' \
  -F 'file=@/tmp/llmgateway-artifact-spoof.pdf;type=image/png')
assert_json_error 415 mime_type_mismatch "$STATUS"

python3 - <<'PY'
with open("/tmp/llmgateway-artifact-big.bin","wb") as f:
    f.write(b"x"*1025)
PY
STATUS=$(curl -sS -D "$ERROR_HEADERS" -o "$ERROR_BODY" -w '%{http_code}' -X POST \
  http://127.0.0.1:7331/v1/files \
  -H "Authorization: Bearer ${LLMGATEWAY_FILE_CLIENT_A_KEY}" \
  -F 'purpose=assistants' \
  -F 'file=@/tmp/llmgateway-artifact-big.bin;type=application/octet-stream')
assert_json_error 413 file_too_large "$STATUS"

curl -fsS -X DELETE "http://127.0.0.1:7331/v1/files/${FILE_A_ID}" \
  -H "Authorization: Bearer ${LLMGATEWAY_FILE_CLIENT_A_KEY}" | grep -q '"deleted":true'
curl -fsS "http://127.0.0.1:7331/v1/files/${FILE_B_ID}/content" \
  -H "Authorization: Bearer ${LLMGATEWAY_FILE_CLIENT_A_KEY}" \
  -o /tmp/llmgateway-artifact-download.txt
cmp /tmp/llmgateway-artifact-b.txt /tmp/llmgateway-artifact-download.txt
rm -f /tmp/llmgateway-artifact-download.txt
test "$(find data/artifacts/blobs -type f | wc -l | tr -d ' ')" = "1"

curl -fsS -X DELETE "http://127.0.0.1:7331/v1/files/${FILE_B_ID}" \
  -H "Authorization: Bearer ${LLMGATEWAY_FILE_CLIENT_A_KEY}" | grep -q '"deleted":true'
test "$(find data/artifacts/blobs -type f | wc -l | tr -d ' ')" = "0"

curl -fsS -X POST http://127.0.0.1:7331/v1/chat/completions \
  -H "Authorization: Bearer ${LLMGATEWAY_API_KEY}" \
  -H "Content-Type: application/json" \
  -d '{"messages":[{"role":"user","content":"legacy default model compatibility"}]}' \
  | grep -q 'fake reply messages=1'

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
