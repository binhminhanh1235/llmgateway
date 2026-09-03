#!/usr/bin/env bash
set -euo pipefail

export LLMGATEWAY_API_KEY="ci-retrieval-key"
export FAKE_API_KEY="fake-key"
export LLMGATEWAY_CONFIG="/tmp/llmgateway-retrieval.toml"

rm -f data/llmgateway.db data/llmgateway.db-shm data/llmgateway.db-wal
mkdir -p data

cat >/tmp/llmgateway-retrieval.toml <<'EOF'
[server]
host = "127.0.0.1"
port = 7332

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
retrieval_top_k = 3
retrieval_max_tokens = 256
retrieval_min_score = 0.05

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
EOF

python3 scripts/fake-openai.py >/tmp/llmgateway-retrieval-fake.log 2>&1 &
FAKE_PID=$!
cargo build --quiet
./target/debug/llmgateway >/tmp/llmgateway-retrieval.log 2>&1 &
PID=$!
trap 'kill "$PID" "$FAKE_PID" 2>/dev/null || true; rm -f data/llmgateway.db data/llmgateway.db-shm data/llmgateway.db-wal /tmp/llmgateway-retrieval.toml /tmp/retrieval-headers' EXIT

for _ in {1..40}; do
  if curl -fsS http://127.0.0.1:7332/_llmgateway/health >/dev/null; then
    break
  fi
  sleep 0.2
done

THREAD_JSON=$(curl -fsS -X POST \
  http://127.0.0.1:7332/v1/threads \
  -H "Authorization: Bearer ${LLMGATEWAY_API_KEY}" \
  -H "Content-Type: application/json" \
  -d '{
    "title":"Retrieval CI",
    "model":"llmgateway-auto",
    "messages":[
      {"role":"user","content":"We chose optimistic locking for payment updates because conflicts are rare."},
      {"role":"assistant","content":"Use a version column and retry conflicting updates."},
      {"role":"user","content":"Kafka zero copy uses sendfile to reduce user-space copying."},
      {"role":"assistant","content":"Yes, that reduces memory copies on the hot path."},
      {"role":"user","content":"Recent unrelated note about deployment."},
      {"role":"assistant","content":"Deployment note acknowledged."}
    ]
  }')
THREAD_ID=$(printf '%s' "$THREAD_JSON" | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')

COMPACTED=$(curl -fsS -X POST \
  "http://127.0.0.1:7332/v1/threads/${THREAD_ID}/compact" \
  -H "Authorization: Bearer ${LLMGATEWAY_API_KEY}" \
  -H "Content-Type: application/json" \
  -d '{}')
printf '%s' "$COMPACTED" | python3 -c '
import json,sys
x=json.load(sys.stdin)
assert x["checkpoint"]["through_ordinal"] == 4, x
'

RETRIEVAL=$(curl -fsS \
  "http://127.0.0.1:7332/v1/threads/${THREAD_ID}/retrieval?q=optimistic%20locking" \
  -H "Authorization: Bearer ${LLMGATEWAY_API_KEY}")
printf '%s' "$RETRIEVAL" | python3 -c '
import json,sys
x=json.load(sys.stdin)
assert x["scorer"] == "lexical-v1", x
assert x["through_ordinal"] == 4, x
assert x["data"], x
assert "optimistic locking" in x["data"][0]["text"].lower(), x
assert x["data"][0]["start_ordinal"] == 1, x
'

ANSWER=$(curl -fsS -D /tmp/retrieval-headers -X POST \
  "http://127.0.0.1:7332/v1/threads/${THREAD_ID}/messages" \
  -H "Authorization: Bearer ${LLMGATEWAY_API_KEY}" \
  -H "Content-Type: application/json" \
  -d '{"content":"Why did we choose optimistic locking?","stream":false}')
printf '%s' "$ANSWER" | grep -q 'fake reply messages=5'
grep -qi '^x-llmgateway-retrieved-chunks: 1' /tmp/retrieval-headers

DETAIL=$(curl -fsS "http://127.0.0.1:7332/v1/threads/${THREAD_ID}" \
  -H "Authorization: Bearer ${LLMGATEWAY_API_KEY}")
printf '%s' "$DETAIL" | python3 -c '
import json,sys
x=json.load(sys.stdin)
assert len(x["messages"]) == 8, x
assert x["messages"][0]["message"]["content"].startswith("We chose optimistic locking"), x
assert x["messages"][6]["message"]["content"] == "Why did we choose optimistic locking?", x
assert x["messages"][7]["message"]["content"] == "fake reply messages=5", x
'

echo "llmgateway semantic retrieval smoke test passed"
