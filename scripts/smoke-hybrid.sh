#!/usr/bin/env bash
set -euo pipefail

export LLMGATEWAY_API_KEY="ci-local-key"
export FAKE_API_KEY="fake-key"
export FAKE_EMBED_LOG="/tmp/llmgateway-fake-embeddings.log"
export LLMGATEWAY_CONFIG="/tmp/llmgateway-hybrid-smoke.toml"

rm -f data/llmgateway.db data/llmgateway.db-shm data/llmgateway.db-wal "$FAKE_EMBED_LOG"
mkdir -p data

cat >/tmp/llmgateway-hybrid-smoke.toml <<'EOF'
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
target_tokens = 2048
reserve_output_tokens = 256
recent_messages = 2
compaction_trigger_ratio = 0.9
summary_input_tokens = 512
summary_max_tokens = 128
summary_model = "llmgateway-auto"
retrieval_enabled = true
retrieval_max_chunks = 2
retrieval_max_tokens = 700
retrieval_min_score = 0.75
retrieval_backend = "hybrid"
retrieval_embedding_account = "fake-primary"
retrieval_embedding_model = "fake-embedding"
retrieval_semantic_weight = 1.0
retrieval_min_similarity = 0.20
retrieval_embedding_batch_size = 64

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

python3 scripts/fake-openai.py >/tmp/llmgateway-hybrid-fake.log 2>&1 &
FAKE_PID=$!
cargo build --quiet
./target/debug/llmgateway >/tmp/llmgateway-hybrid.log 2>&1 &
PID=$!
trap 'kill "$PID" "$FAKE_PID" 2>/dev/null || true; rm -f data/llmgateway.db data/llmgateway.db-shm data/llmgateway.db-wal /tmp/llmgateway-hybrid-smoke.toml "$FAKE_EMBED_LOG"' EXIT

for _ in {1..40}; do
  if curl -fsS http://127.0.0.1:7331/_llmgateway/health >/dev/null; then
    break
  fi
  sleep 0.2
done

THREAD_JSON=$(curl -fsS -X POST http://127.0.0.1:7331/v1/threads \
  -H "Authorization: Bearer ${LLMGATEWAY_API_KEY}" \
  -H "Content-Type: application/json" \
  -d '{
    "title":"Hybrid retrieval CI",
    "model":"llmgateway-auto",
    "messages":[
      {"role":"user","content":"For concurrent invoice updates we chose optimistic locking with a version column."},
      {"role":"assistant","content":"A stale writer gets a conflict and retries instead of silently overwriting another update."},
      {"role":"user","content":"Kafka zero copy uses sendfile."},
      {"role":"assistant","content":"That reduces user-space copying between the page cache and socket."},
      {"role":"user","content":"The deployment uses a small container rollout."},
      {"role":"assistant","content":"The rollout is unrelated to database concurrency."}
    ]
  }')
THREAD_ID=$(printf '%s' "$THREAD_JSON" | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')

curl -fsS -X POST "http://127.0.0.1:7331/v1/threads/${THREAD_ID}/compact" \
  -H "Authorization: Bearer ${LLMGATEWAY_API_KEY}" \
  -H "Content-Type: application/json" \
  -d '{}' >/tmp/hybrid-compact.json
python3 - <<'PY'
import json
x=json.load(open('/tmp/hybrid-compact.json'))
assert x['checkpoint']['through_ordinal'] == 4, x
PY

curl -fsS -D /tmp/hybrid-first.headers -o /tmp/hybrid-first.json -X POST \
  "http://127.0.0.1:7331/v1/threads/${THREAD_ID}/messages" \
  -H "Authorization: Bearer ${LLMGATEWAY_API_KEY}" \
  -H "Content-Type: application/json" \
  -d '{"content":"How did we prevent lost updates?","stream":false}'

grep -qi '^x-llmgateway-retrieval-backend: hybrid' /tmp/hybrid-first.headers
grep -qi '^x-llmgateway-retrieved-chunks: 1' /tmp/hybrid-first.headers
grep -q 'retrieval=yes' /tmp/hybrid-first.json

python3 - <<'PY'
import json
rows=[json.loads(line) for line in open('/tmp/llmgateway-fake-embeddings.log') if line.strip()]
counts=[row['count'] for row in rows]
assert counts == [1, 2], counts
PY

curl -fsS -D /tmp/hybrid-second.headers -o /tmp/hybrid-second.json -X POST \
  "http://127.0.0.1:7331/v1/threads/${THREAD_ID}/messages" \
  -H "Authorization: Bearer ${LLMGATEWAY_API_KEY}" \
  -H "Content-Type: application/json" \
  -d '{"content":"Remind me again how lost updates were prevented.","stream":false}'

grep -qi '^x-llmgateway-retrieval-backend: hybrid' /tmp/hybrid-second.headers
grep -q 'retrieval=yes' /tmp/hybrid-second.json

python3 - <<'PY'
import json
rows=[json.loads(line) for line in open('/tmp/llmgateway-fake-embeddings.log') if line.strip()]
counts=[row['count'] for row in rows]
# First request embeds query + two historical chunks. Second request embeds only its query;
# the historical chunk vectors must come from SQLite cache.
assert counts == [1, 2, 1], counts
PY

echo "llmgateway hybrid retrieval smoke test passed"
