#!/usr/bin/env bash
set -euo pipefail

export LLMGATEWAY_API_KEY="ci-local-key"
export FAKE_PRIMARY_KEY="quota429"
export FAKE_SECONDARY_KEY="healthy"
export LLMGATEWAY_CONFIG="/tmp/llmgateway-quota-smoke.toml"

rm -f data/llmgateway.db data/llmgateway.db-shm data/llmgateway.db-wal
mkdir -p data

cat >/tmp/llmgateway-quota-smoke.toml <<'EOF'
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

[usage.accounts.fake-primary]
daily_request_limit = 100
monthly_request_limit = 1000
rate_limit_cooldown_seconds = 120

[usage.accounts.fake-secondary]
daily_request_limit = 100
monthly_request_limit = 1000

[context]
enabled = false
retrieval_enabled = false

[[providers]]
id = "fake"
kind = "openai-compatible"
base_url = "http://127.0.0.1:18080/v1"
models_path = "models"

[[accounts]]
id = "fake-primary"
provider = "fake"
api_key_env = "FAKE_PRIMARY_KEY"
auth_style = "bearer"
enabled = true
discover_models = false

[[accounts]]
id = "fake-secondary"
provider = "fake"
api_key_env = "FAKE_SECONDARY_KEY"
auth_style = "bearer"
enabled = true
discover_models = false

[[routes]]
id = "primary-route"
account = "fake-primary"
model = "fake-model"
priority = 10
enabled = true
capabilities = ["chat"]

[[routes]]
id = "secondary-route"
account = "fake-secondary"
model = "fake-model"
priority = 20
enabled = true
capabilities = ["chat"]

[virtual_models.llmgateway-auto]
routes = ["primary-route", "secondary-route"]

[virtual_models.llmgateway-coding]
routes = ["primary-route", "secondary-route"]

[virtual_models.llmgateway-best]
routes = ["primary-route", "secondary-route"]
EOF

python3 scripts/fake-openai.py >/tmp/llmgateway-quota-fake.log 2>&1 &
FAKE_PID=$!
cargo build --quiet
./target/debug/llmgateway >/tmp/llmgateway-quota.log 2>&1 &
PID=$!
trap 'kill "$PID" "$FAKE_PID" 2>/dev/null || true; rm -f data/llmgateway.db data/llmgateway.db-shm data/llmgateway.db-wal /tmp/llmgateway-quota-smoke.toml' EXIT

for _ in {1..60}; do
  if curl -fsS http://127.0.0.1:7331/_llmgateway/health >/dev/null; then
    break
  fi
  sleep 0.2
done

curl -fsS -D /tmp/quota-first.headers -o /tmp/quota-first.json \
  -X POST http://127.0.0.1:7331/v1/chat/completions \
  -H "Authorization: Bearer ${LLMGATEWAY_API_KEY}" \
  -H "Content-Type: application/json" \
  -d '{"model":"llmgateway-auto","stream":false,"messages":[{"role":"user","content":"quota failover one"}]}'
grep -qi '^x-llmgateway-route: secondary-route' /tmp/quota-first.headers

USAGE_ONE=$(curl -fsS http://127.0.0.1:7331/_llmgateway/usage \
  -H "Authorization: Bearer ${LLMGATEWAY_API_KEY}")
printf '%s' "$USAGE_ONE" | python3 -c '
import json,sys
x=json.load(sys.stdin)
accounts={a["account_id"]:a for a in x["accounts"]}
p=accounts["fake-primary"]
s=accounts["fake-secondary"]
assert p["blocked"] is True, p
assert p["consecutive_429"] == 1, p
assert p["daily"]["requests"] == 1, p
assert p["remaining_requests_hint"] == 0, p
assert s["blocked"] is False, s
assert s["daily"]["requests"] == 1, s
assert s["remaining_requests_hint"] == 99, s
'

curl -fsS -D /tmp/quota-second.headers -o /tmp/quota-second.json \
  -X POST http://127.0.0.1:7331/v1/chat/completions \
  -H "Authorization: Bearer ${LLMGATEWAY_API_KEY}" \
  -H "Content-Type: application/json" \
  -d '{"model":"llmgateway-auto","stream":false,"messages":[{"role":"user","content":"quota failover two"}]}'
grep -qi '^x-llmgateway-route: secondary-route' /tmp/quota-second.headers

PRIMARY=$(curl -fsS http://127.0.0.1:7331/_llmgateway/accounts/fake-primary/usage \
  -H "Authorization: Bearer ${LLMGATEWAY_API_KEY}")
SECONDARY=$(curl -fsS http://127.0.0.1:7331/_llmgateway/accounts/fake-secondary/usage \
  -H "Authorization: Bearer ${LLMGATEWAY_API_KEY}")
printf '%s' "$PRIMARY" | python3 -c '
import json,sys
x=json.load(sys.stdin)
assert x["daily"]["requests"] == 1, x
assert x["blocked"] is True, x
'
printf '%s' "$SECONDARY" | python3 -c '
import json,sys
x=json.load(sys.stdin)
assert x["daily"]["requests"] == 2, x
assert x["blocked"] is False, x
assert x["daily"]["tokens"] > 0, x
'

RESET=$(curl -fsS -X POST http://127.0.0.1:7331/_llmgateway/accounts/fake-primary/quota/reset \
  -H "Authorization: Bearer ${LLMGATEWAY_API_KEY}")
printf '%s' "$RESET" | python3 -c '
import json,sys
x=json.load(sys.stdin)
assert x["reset"] is True, x
'

PRIMARY_RESET=$(curl -fsS http://127.0.0.1:7331/_llmgateway/accounts/fake-primary/usage \
  -H "Authorization: Bearer ${LLMGATEWAY_API_KEY}")
printf '%s' "$PRIMARY_RESET" | python3 -c '
import json,sys
x=json.load(sys.stdin)
assert x["blocked"] is False, x
assert x["consecutive_429"] == 0, x
assert x["remaining_requests_hint"] is None, x
assert x["daily"]["requests"] == 1, x
'

echo "llmgateway quota usage smoke test passed"
