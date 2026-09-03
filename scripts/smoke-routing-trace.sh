#!/usr/bin/env bash
set -euo pipefail

export LLMGATEWAY_API_KEY="ci-local-key"
unset MISSING_TRACE_KEY || true
export TRACE_PRIMARY_KEY="quota429"
export TRACE_SECONDARY_KEY="healthy"
export LLMGATEWAY_CONFIG="/tmp/llmgateway-routing-trace.toml"

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

[usage]
enabled = true
hard_limits = true
default_rate_limit_cooldown_seconds = 60
balance_weight = 0

[usage.accounts.trace-primary]
daily_request_limit = 100
monthly_request_limit = 1000
rate_limit_cooldown_seconds = 120

[usage.accounts.trace-secondary]
daily_request_limit = 100
monthly_request_limit = 1000

[context]
enabled = false
retrieval_enabled = false

[[providers]]
id = "fake"
kind = "openai-compatible"
base_url = "http://127.0.0.1:18080/v1"

[[accounts]]
id = "trace-missing"
provider = "fake"
api_key_env = "MISSING_TRACE_KEY"
enabled = true
discover_models = false

[[accounts]]
id = "trace-primary"
provider = "fake"
api_key_env = "TRACE_PRIMARY_KEY"
enabled = true
discover_models = false

[[accounts]]
id = "trace-secondary"
provider = "fake"
api_key_env = "TRACE_SECONDARY_KEY"
enabled = true
discover_models = false

[[routes]]
id = "missing-route"
account = "trace-missing"
model = "fake-model"
priority = 0
enabled = true
capabilities = ["chat"]

[[routes]]
id = "primary-route"
account = "trace-primary"
model = "fake-model"
priority = 10
enabled = true
capabilities = ["chat"]

[[routes]]
id = "secondary-route"
account = "trace-secondary"
model = "fake-model"
priority = 20
enabled = true
capabilities = ["chat"]

[virtual_models.llmgateway-auto]
routes = ["missing-route", "primary-route", "secondary-route"]
EOF

python3 scripts/fake-openai.py >/tmp/llmgateway-routing-trace-fake.log 2>&1 &
FAKE_PID=$!
cargo build --quiet
./target/debug/llmgateway >/tmp/llmgateway-routing-trace.log 2>&1 &
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

BEFORE=$(curl -fsS -X POST http://127.0.0.1:7331/_llmgateway/routes/explain \
  -H "Authorization: Bearer ${LLMGATEWAY_API_KEY}" \
  -H "Content-Type: application/json" \
  -d '{"model":"llmgateway-auto"}')
printf '%s' "$BEFORE" | python3 -c '
import json,sys
x=json.load(sys.stdin)
assert x["requested_model"] == "llmgateway-auto", x
assert x["resolved_model"] == "llmgateway-auto", x
assert x["selected_route"] == "primary-route", x
c={item["route_id"]: item for item in x["candidates"]}
missing=c["missing-route"]
primary=c["primary-route"]
secondary=c["secondary-route"]
assert missing["eligible"] is False, missing
assert "credential_missing" in missing["exclusion_reasons"], missing
assert missing["rank"] is None, missing
assert primary["eligible"] is True and primary["selected"] is True, primary
assert primary["rank"] == 1 and primary["final_score"] == 10, primary
assert primary["quota_penalty"] == 0, primary
assert secondary["eligible"] is True and secondary["rank"] == 2, secondary
assert secondary["final_score"] == 20, secondary
'

curl -fsS -D /tmp/routing-trace-chat.headers -o /tmp/routing-trace-chat.json \
  -X POST http://127.0.0.1:7331/v1/chat/completions \
  -H "Authorization: Bearer ${LLMGATEWAY_API_KEY}" \
  -H "Content-Type: application/json" \
  -d '{"model":"llmgateway-auto","stream":false,"messages":[{"role":"user","content":"trigger route failover"}]}'
grep -qi '^x-llmgateway-route: secondary-route' /tmp/routing-trace-chat.headers

AFTER=$(curl -fsS -X POST http://127.0.0.1:7331/_llmgateway/routes/explain \
  -H "Authorization: Bearer ${LLMGATEWAY_API_KEY}" \
  -H "Content-Type: application/json" \
  -d '{"model":"llmgateway-auto"}')
printf '%s' "$AFTER" | python3 -c '
import json,sys
x=json.load(sys.stdin)
assert x["selected_route"] == "secondary-route", x
c={item["route_id"]: item for item in x["candidates"]}
primary=c["primary-route"]
secondary=c["secondary-route"]
assert primary["eligible"] is False, primary
assert primary["selected"] is False, primary
assert primary["rank"] is None, primary
assert "route_cooldown" in primary["exclusion_reasons"], primary
assert "quota_blocked" in primary["exclusion_reasons"], primary
assert primary["route_health"]["consecutive_failures"] >= 1, primary
assert primary["route_health"]["cooldown_until"], primary
assert secondary["eligible"] is True and secondary["selected"] is True, secondary
assert secondary["rank"] == 1, secondary
'

echo "llmgateway routing decision trace smoke test passed"
