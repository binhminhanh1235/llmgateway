#!/usr/bin/env bash
set -euo pipefail

export LLMGATEWAY_API_KEY="ci-local-key"
export TASK_KEY="healthy"
export LLMGATEWAY_CONFIG="/tmp/llmgateway-task-aware-routing.toml"

rm -f data/llmgateway.db data/llmgateway.db-shm data/llmgateway.db-wal
mkdir -p data

cat >"$LLMGATEWAY_CONFIG" <<'EOF'
[server]
host = "127.0.0.1"
port = 7331

[api]
key_env = "LLMGATEWAY_API_KEY"
default_model = "llmgateway-auto"

[storage]
database_url = "sqlite://data/llmgateway.db"

[routing]
adaptive_enabled = false
task_aware_enabled = true
task_fit_max_bonus = 20
task_mismatch_penalty = 12
task_long_context_threshold_tokens = 12000
task_simple_max_input_tokens = 800

[context]
enabled = false
retrieval_enabled = false

[[providers]]
id = "fake"
kind = "openai-compatible"
base_url = "http://127.0.0.1:18080/v1"

[[accounts]]
id = "task-account"
provider = "fake"
api_key_env = "TASK_KEY"
enabled = true
discover_models = false

[[routes]]
id = "cheap-route"
account = "task-account"
model = "cheap-model"
priority = 10
enabled = true
capabilities = ["chat", "cheap", "fast"]
context_window = 4096

[[routes]]
id = "coder-route"
account = "task-account"
model = "coder-model"
priority = 20
enabled = true
capabilities = ["chat", "coding", "reasoning"]
context_window = 8192

[[routes]]
id = "long-route"
account = "task-account"
model = "long-model"
priority = 25
enabled = true
capabilities = ["chat", "reasoning", "long-context"]
context_window = 65536

[virtual_models.llmgateway-auto]
routes = ["cheap-route", "coder-route", "long-route"]
EOF

python3 scripts/fake-openai.py >/tmp/llmgateway-task-aware-fake.log 2>&1 &
FAKE_PID=$!
cargo build --quiet
./target/debug/llmgateway >/tmp/llmgateway-task-aware.log 2>&1 &
PID=$!
cleanup() {
  kill "$PID" "$FAKE_PID" 2>/dev/null || true
  rm -f data/llmgateway.db data/llmgateway.db-shm data/llmgateway.db-wal "$LLMGATEWAY_CONFIG"
}
trap cleanup EXIT

for _ in {1..60}; do
  if curl -fsS http://127.0.0.1:7331/_llmgateway/health >/dev/null; then
    break
  fi
  sleep 0.2
done

CODING=$(curl -fsS -X POST http://127.0.0.1:7331/_llmgateway/routes/explain   -H "Authorization: Bearer $LLMGATEWAY_API_KEY"   -H "Content-Type: application/json"   -d '{"model":"llmgateway-auto","body":{"messages":[{"role":"user","content":"Implement a Rust retry function and debug this compiler error: ```rust\nfn broken( {\n```"}]}}')
printf '%s' "$CODING" | python3 -c '
import json,sys
x=json.load(sys.stdin)
assert x["task"]["kind"] == "coding", x
assert x["selected_route"] == "coder-route", x
c={item["route_id"]: item for item in x["candidates"]}
assert c["coder-route"]["task_adjustment"] == -20, c["coder-route"]
assert "coding" in c["coder-route"]["task_fit"]["matched_capabilities"], c["coder-route"]
assert c["coder-route"]["final_score"] == 0, c["coder-route"]
assert c["cheap-route"]["final_score"] == 10, c["cheap-route"]
'

SIMPLE=$(curl -fsS -X POST http://127.0.0.1:7331/_llmgateway/routes/explain   -H "Authorization: Bearer $LLMGATEWAY_API_KEY"   -H "Content-Type: application/json"   -d '{"model":"llmgateway-auto","body":{"messages":[{"role":"user","content":"hello"}]}}')
printf '%s' "$SIMPLE" | python3 -c '
import json,sys
x=json.load(sys.stdin)
assert x["task"]["kind"] == "simple_chat", x
assert x["selected_route"] == "cheap-route", x
c={item["route_id"]: item for item in x["candidates"]}
assert c["cheap-route"]["task_adjustment"] == -20, c["cheap-route"]
assert c["cheap-route"]["final_score"] == -10, c["cheap-route"]
assert "cheap" in c["cheap-route"]["task_fit"]["matched_capabilities"], c["cheap-route"]
'

LONG=$(curl -fsS -X POST http://127.0.0.1:7331/_llmgateway/routes/explain   -H "Authorization: Bearer $LLMGATEWAY_API_KEY"   -H "Content-Type: application/json"   -d '{"model":"llmgateway-auto","task":"long_context","body":{"max_tokens":10000,"messages":[{"role":"user","content":"summarize the attached material"}]}}')
printf '%s' "$LONG" | python3 -c '
import json,sys
x=json.load(sys.stdin)
assert x["task"]["kind"] == "long_context", x
assert x["task"]["explicit"] is True, x
assert x["selected_route"] == "long-route", x
c={item["route_id"]: item for item in x["candidates"]}
for route_id in ["cheap-route", "coder-route"]:
    route=c[route_id]
    assert route["eligible"] is False, route
    assert "context_window_too_small" in route["exclusion_reasons"], route
    assert route["task_fit"]["context_sufficient"] is False, route
assert c["long-route"]["eligible"] is True, c["long-route"]
assert c["long-route"]["task_fit"]["context_sufficient"] is True, c["long-route"]
assert "long-context" in c["long-route"]["task_fit"]["matched_capabilities"], c["long-route"]
'

curl -fsS -D /tmp/task-aware-chat.headers -o /tmp/task-aware-chat.json   -X POST http://127.0.0.1:7331/v1/chat/completions   -H "Authorization: Bearer $LLMGATEWAY_API_KEY"   -H "Content-Type: application/json"   -d '{"model":"llmgateway-auto","llmgateway_task":"coding","stream":false,"messages":[{"role":"user","content":"implement retry"}]}'
grep -qi '^x-llmgateway-route: coder-route' /tmp/task-aware-chat.headers

echo "llmgateway task-aware routing smoke test passed"
