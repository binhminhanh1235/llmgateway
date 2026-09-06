#!/usr/bin/env bash
set -euo pipefail

export LLMGATEWAY_API_KEY="ci-agent-admin"
export AGENT_RESTRICTED_KEY="ci-agent-restricted"
export FAKE_API_KEY="healthy"
export LLMGATEWAY_CONFIG="/tmp/llmgateway-agent-control.toml"

DB="data/llmgateway-agent-control.db"
rm -f "$DB" "$DB-shm" "$DB-wal"
mkdir -p data

cat >"$LLMGATEWAY_CONFIG" <<'EOF'
[server]
host = "127.0.0.1"
port = 7331

[api]
key_env = "LLMGATEWAY_API_KEY"
default_model = "llmgateway-auto"

[storage]
database_url = "sqlite://data/llmgateway-agent-control.db"

[clients.restricted]
key_env = "AGENT_RESTRICTED_KEY"
enabled = true
allowed_models = ["llmgateway-auto"]
allowed_routes = ["small-route"]
execution_preference = "balanced"
api_fallback = true

[routing]
adaptive_enabled = false
task_aware_enabled = false
execution_preference = "balanced"
api_fallback = true

[context]
enabled = false
retrieval_enabled = false

[[providers]]
id = "fake"
kind = "openai-compatible"
base_url = "http://127.0.0.1:18080/v1"

[[accounts]]
id = "fake-account"
provider = "fake"
api_key_env = "FAKE_API_KEY"
enabled = true
discover_models = false

[[routes]]
id = "small-route"
account = "fake-account"
model = "fake-small"
priority = 10
enabled = true
capabilities = ["chat", "fast"]
context_window = 4096

[[routes]]
id = "coder-route"
account = "fake-account"
model = "fake-coder"
priority = 20
enabled = true
capabilities = ["chat", "coding", "reasoning"]
context_window = 65536

[[routes]]
id = "long-route"
account = "fake-account"
model = "fake-long"
priority = 30
enabled = true
capabilities = ["chat", "long-context"]
context_window = 131072

[virtual_models.llmgateway-auto]
routes = ["small-route", "coder-route", "long-route"]
EOF

python3 scripts/fake-openai.py >/tmp/llmgateway-agent-control-fake.log 2>&1 &
FAKE_PID=$!
cargo build --quiet
./target/debug/llmgateway >/tmp/llmgateway-agent-control.log 2>&1 &
PID=$!

cleanup() {
  kill "$PID" "$FAKE_PID" 2>/dev/null || true
  rm -f "$LLMGATEWAY_CONFIG"
  rm -f "$DB" "$DB-shm" "$DB-wal"
}
trap cleanup EXIT

for _ in {1..80}; do
  if curl -fsS http://127.0.0.1:7331/_llmgateway/health >/dev/null; then
    break
  fi
  sleep 0.2
done

ADMIN=(-H "Authorization: Bearer ${LLMGATEWAY_API_KEY}")
RESTRICTED_AUTH=(-H "Authorization: Bearer ${AGENT_RESTRICTED_KEY}")
JSON=(-H "Content-Type: application/json")

CAPABILITIES=$(curl -fsS \
  http://127.0.0.1:7331/_llmgateway/agent/capabilities \
  "${ADMIN[@]}")
printf '%s' "$CAPABILITIES" | python3 -c '
import json,sys
x=json.load(sys.stdin)
assert x["object"] == "llmgateway.agent.capabilities", x
vm=next(m for m in x["models"] if m["id"] == "llmgateway-auto")
assert vm["kind"] == "virtual", vm
assert {"coding","reasoning","long-context"} <= set(vm["capabilities"]), vm
assert vm["eligible_routes"] == 3, vm
'

CODING=$(curl -fsS -X POST \
  http://127.0.0.1:7331/_llmgateway/agent/resolve \
  "${ADMIN[@]}" "${JSON[@]}" \
  -d '{"model":"llmgateway-auto","requirements":{"capabilities":["coding"]}}')
printf '%s' "$CODING" | python3 -c '
import json,sys
x=json.load(sys.stdin)
assert x["status"] == "resolved", x
assert x["selected"]["route_id"] == "coder-route", x
small=next(c for c in x["candidates"] if c["route_id"] == "small-route")
assert small["eligible"] is False, small
assert "required_capability_missing" in small["exclusion_reasons"], small
assert small["missing_required_capabilities"] == ["coding"], small
'

LONG=$(curl -fsS -X POST \
  http://127.0.0.1:7331/_llmgateway/agent/resolve \
  "${ADMIN[@]}" "${JSON[@]}" \
  -d '{"requirements":{"min_context_window":100000}}')
printf '%s' "$LONG" | python3 -c '
import json,sys
x=json.load(sys.stdin)
assert x["selected"]["route_id"] == "long-route", x
assert x["requirements"]["min_context_window"] == 100000, x
'

IMPOSSIBLE=$(curl -fsS -X POST \
  http://127.0.0.1:7331/_llmgateway/agent/diagnostics \
  "${ADMIN[@]}" "${JSON[@]}" \
  -d '{"requirements":{"capabilities":["vision"]}}')
printf '%s' "$IMPOSSIBLE" | python3 -c '
import json,sys
x=json.load(sys.stdin)
assert x["status"] == "blocked", x
assert x["resolution"]["status"] == "unresolved", x
assert x["blocking_reasons"]["required_capability_missing"] == 3, x
assert x["recommended_action"] == "relax_requirements_or_enable_capable_model", x
'

RESTRICTED_RESULT=$(curl -fsS -X POST \
  http://127.0.0.1:7331/_llmgateway/agent/resolve \
  "${RESTRICTED_AUTH[@]}" "${JSON[@]}" \
  -d '{"model":"llmgateway-auto","requirements":{"capabilities":["coding"]}}')
printf '%s' "$RESTRICTED_RESULT" | python3 -c '
import json,sys
x=json.load(sys.stdin)
assert x["status"] == "unresolved", x
assert x["client_id"] == "restricted", x
assert "client_policy_route_forbidden" in x["blocking_reasons"], x
coder=next(c for c in x["candidates"] if c["route_id"] == "coder-route")
assert coder["eligible"] is False, coder
assert "client_policy_route_forbidden" in coder["exclusion_reasons"], coder
'

curl -fsS -D /tmp/llmgateway-agent-control.headers \
  -o /tmp/llmgateway-agent-control.chat.json \
  -X POST http://127.0.0.1:7331/v1/chat/completions \
  "${ADMIN[@]}" "${JSON[@]}" \
  -d '{"model":"llmgateway-auto","llmgateway_requirements":{"capabilities":["coding"]},"messages":[{"role":"user","content":"hello"}]}'
grep -qi '^x-llmgateway-route: coder-route' /tmp/llmgateway-agent-control.headers

echo "llmgateway Agent Control API + capability routing smoke test passed"
