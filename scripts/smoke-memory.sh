#!/usr/bin/env bash
set -euo pipefail

export LLMGATEWAY_API_KEY="ci-local-key"
export FAKE_API_KEY="fake-key"
export LLMGATEWAY_CONFIG="/tmp/llmgateway-memory-smoke.toml"

rm -f data/llmgateway.db data/llmgateway.db-shm data/llmgateway.db-wal
mkdir -p data

cat >/tmp/llmgateway-memory-smoke.toml <<'EOF'
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
compaction_trigger_ratio = 0.9
summary_input_tokens = 512
summary_max_tokens = 128
summary_model = "llmgateway-auto"
retrieval_enabled = false

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

python3 scripts/fake-openai.py >/tmp/llmgateway-memory-fake.log 2>&1 &
FAKE_PID=$!
cargo build --quiet
./target/debug/llmgateway >/tmp/llmgateway-memory.log 2>&1 &
PID=$!
trap 'kill "$PID" "$FAKE_PID" 2>/dev/null || true; rm -f data/llmgateway.db data/llmgateway.db-shm data/llmgateway.db-wal /tmp/llmgateway-memory-smoke.toml' EXIT

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
    "title":"Memory provenance CI",
    "model":"llmgateway-auto",
    "messages":[
      {"role":"user","content":"We are building llmgateway."},
      {"role":"assistant","content":"The full transcript stays in SQLite."},
      {"role":"user","content":"Keep the service local-first."},
      {"role":"assistant","content":"That is a stable preference."}
    ]
  }')
THREAD_ID=$(printf '%s' "$THREAD_JSON" | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')

COMPACTED=$(curl -fsS -X POST \
  "http://127.0.0.1:7331/v1/threads/${THREAD_ID}/compact" \
  -H "Authorization: Bearer ${LLMGATEWAY_API_KEY}" \
  -H "Content-Type: application/json" \
  -d '{}')
printf '%s' "$COMPACTED" | python3 -c '
import json,sys
x=json.load(sys.stdin)
assert x["checkpoint"]["through_ordinal"] == 2, x
assert x["memory"] is not None, x
'

MEMORY=$(curl -fsS "http://127.0.0.1:7331/v1/threads/${THREAD_ID}/memory" \
  -H "Authorization: Bearer ${LLMGATEWAY_API_KEY}")
printf '%s' "$MEMORY" | python3 -c '
import json,sys
x=json.load(sys.stdin)
items=x["items"]
assert items, x
fact=next(i for i in items if i["category"] == "fact")
assert fact["source_kind"] == "checkpoint", fact
assert fact["confidence"] == 0.65, fact
assert fact["first_seen_ordinal"] == 2, fact
assert fact["last_seen_ordinal"] == 2, fact
assert fact["pinned"] is False, fact
'

PIN=$(curl -fsS -X POST \
  "http://127.0.0.1:7331/v1/threads/${THREAD_ID}/memory/pins" \
  -H "Authorization: Bearer ${LLMGATEWAY_API_KEY}" \
  -H "Content-Type: application/json" \
  -d '{"category":"constraint","value":"Never expose the gateway publicly without TLS","confidence":1.0}')
PIN_KEY=$(printf '%s' "$PIN" | python3 -c '
import json,sys
item=json.load(sys.stdin)["item"]
assert item["pinned"] is True, item
assert item["active"] is True, item
assert item["source_kind"] == "manual", item
assert item["confidence"] == 1.0, item
print(item["item_key"])
')

curl -fsS -D /tmp/memory-pin.headers -o /tmp/memory-pin.json -X POST \
  "http://127.0.0.1:7331/v1/threads/${THREAD_ID}/messages" \
  -H "Authorization: Bearer ${LLMGATEWAY_API_KEY}" \
  -H "Content-Type: application/json" \
  -d '{"content":"What constraint must remain authoritative?","stream":false}'
grep -qi '^x-llmgateway-pinned-memory: yes' /tmp/memory-pin.headers
grep -q 'pin=yes' /tmp/memory-pin.json

SECOND_COMPACT=$(curl -fsS -X POST \
  "http://127.0.0.1:7331/v1/threads/${THREAD_ID}/compact" \
  -H "Authorization: Bearer ${LLMGATEWAY_API_KEY}" \
  -H "Content-Type: application/json" \
  -d '{}')
printf '%s' "$SECOND_COMPACT" | python3 -c '
import json,sys
x=json.load(sys.stdin)
assert x["checkpoint"]["through_ordinal"] == 4, x
'

MEMORY_AFTER=$(curl -fsS "http://127.0.0.1:7331/v1/threads/${THREAD_ID}/memory" \
  -H "Authorization: Bearer ${LLMGATEWAY_API_KEY}")
printf '%s' "$MEMORY_AFTER" | python3 -c "
import json,sys
x=json.load(sys.stdin)
pin=next(i for i in x['items'] if i['item_key'] == '${PIN_KEY}')
assert pin['pinned'] is True, pin
assert pin['active'] is True, pin
assert pin['source_kind'] == 'manual', pin
assert pin['confidence'] == 1.0, pin
fact=next(i for i in x['items'] if i['category'] == 'fact')
assert abs(fact['confidence'] - 0.70) < 1e-9, fact
assert fact['last_seen_ordinal'] == 4, fact
"

UNPIN=$(curl -fsS -X PATCH \
  "http://127.0.0.1:7331/v1/threads/${THREAD_ID}/memory/items/${PIN_KEY}" \
  -H "Authorization: Bearer ${LLMGATEWAY_API_KEY}" \
  -H "Content-Type: application/json" \
  -d '{"pinned":false}')
printf '%s' "$UNPIN" | python3 -c '
import json,sys
item=json.load(sys.stdin)["item"]
assert item["pinned"] is False, item
'

curl -fsS -D /tmp/memory-unpin.headers -o /tmp/memory-unpin.json -X POST \
  "http://127.0.0.1:7331/v1/threads/${THREAD_ID}/messages" \
  -H "Authorization: Bearer ${LLMGATEWAY_API_KEY}" \
  -H "Content-Type: application/json" \
  -d '{"content":"Check after unpin","stream":false}'
grep -qi '^x-llmgateway-pinned-memory: no' /tmp/memory-unpin.headers
if grep -q 'pin=yes' /tmp/memory-unpin.json; then
  echo "unexpected pinned memory marker after unpin" >&2
  exit 1
fi

echo "llmgateway memory provenance smoke test passed"
