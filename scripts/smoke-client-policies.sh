#!/usr/bin/env bash
set -euo pipefail

export LLMGATEWAY_API_KEY="ci-admin-key"
export FALLBACK_CLIENT_KEY="ci-fallback-client"
export BROWSER_ONLY_CLIENT_KEY="ci-browser-only-client"
export REQUEST_BUDGET_CLIENT_KEY="ci-request-budget-client"
export TOKEN_BUDGET_CLIENT_KEY="ci-token-budget-client"
export MODEL_CLIENT_KEY="ci-model-client"
export RESPONSE_A_CLIENT_KEY="ci-response-a-client"
export RESPONSE_B_CLIENT_KEY="ci-response-b-client"
export FAKE_API_KEY="healthy"
export LLMGATEWAY_CONFIG="/tmp/llmgateway-client-policies.toml"

DB="data/llmgateway-client-policies.db"
rm -f "$DB" "$DB-shm" "$DB-wal"
mkdir -p data

write_config() {
cat >"$LLMGATEWAY_CONFIG" <<'EOF'
[server]
host = "127.0.0.1"
port = 7331

[api]
key_env = "LLMGATEWAY_API_KEY"
default_model = "llmgateway-auto"

[storage]
database_url = "sqlite://data/llmgateway-client-policies.db"

[clients.fallback]
key_env = "FALLBACK_CLIENT_KEY"
enabled = true
allowed_models = ["llmgateway-auto"]
execution_preference = "prefer-browser"
api_fallback = true

[clients.browser-only]
key_env = "BROWSER_ONLY_CLIENT_KEY"
enabled = true
allowed_models = ["llmgateway-auto"]
allowed_routes = ["browser-*"]
execution_preference = "browser-only"
api_fallback = false

[clients.request-budget]
key_env = "REQUEST_BUDGET_CLIENT_KEY"
enabled = true
allowed_models = ["llmgateway-auto"]
execution_preference = "prefer-browser"
api_fallback = true
daily_request_limit = 1

[clients.token-budget]
key_env = "TOKEN_BUDGET_CLIENT_KEY"
enabled = true
allowed_models = ["llmgateway-auto"]
execution_preference = "prefer-browser"
api_fallback = true
daily_token_limit = 500

[clients.model-limited]
key_env = "MODEL_CLIENT_KEY"
enabled = true
allowed_models = ["allowed-vm"]
execution_preference = "prefer-browser"
api_fallback = true

[clients.response-a]
key_env = "RESPONSE_A_CLIENT_KEY"
enabled = true
allowed_models = ["llmgateway-auto"]
execution_preference = "prefer-browser"
api_fallback = true

[clients.response-b]
key_env = "RESPONSE_B_CLIENT_KEY"
enabled = true
allowed_models = ["llmgateway-auto"]
execution_preference = "prefer-browser"
api_fallback = true

[routing]
adaptive_enabled = false
task_aware_enabled = false
execution_preference = "prefer-browser"
api_fallback = true

[context]
enabled = false
retrieval_enabled = false

[usage]
enabled = true
hard_limits = true

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
id = "api-route"
account = "fake-account"
model = "fake-model"
priority = 10
enabled = true
capabilities = ["chat"]

[virtual_models.llmgateway-auto]
routes = ["api-route"]

[virtual_models.allowed-vm]
routes = ["api-route"]

[virtual_models.blocked-vm]
routes = ["api-route"]
EOF
}

write_legacy_config() {
cat >"$LLMGATEWAY_CONFIG" <<'EOF'
[server]
host = "127.0.0.1"
port = 7331

[api]
key_env = "LLMGATEWAY_API_KEY"
default_model = "llmgateway-auto"

[storage]
database_url = "sqlite://data/llmgateway-client-policies.db"

[routing]
adaptive_enabled = false
task_aware_enabled = false

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
id = "api-route"
account = "fake-account"
model = "fake-model"
priority = 10
enabled = true

[virtual_models.llmgateway-auto]
routes = ["api-route"]
EOF
}

wait_ready() {
  for _ in {1..80}; do
    if curl -fsS http://127.0.0.1:7331/_llmgateway/health >/dev/null; then
      return 0
    fi
    sleep 0.2
  done
  echo "gateway did not become ready" >&2
  return 1
}

start_gateway() {
  ./target/debug/llmgateway >/tmp/llmgateway-client-policies.log 2>&1 &
  PID=$!
  wait_ready
}

stop_gateway() {
  if [[ -n "${PID:-}" ]]; then
    kill "$PID" 2>/dev/null || true
    wait "$PID" 2>/dev/null || true
    PID=""
  fi
}

write_config
python3 scripts/fake-openai.py >/tmp/llmgateway-client-policies-fake.log 2>&1 &
FAKE_PID=$!
cargo build --quiet
PID=""
cleanup() {
  stop_gateway
  kill "$FAKE_PID" 2>/dev/null || true
  wait "$FAKE_PID" 2>/dev/null || true
  rm -f "$DB" "$DB-shm" "$DB-wal" "$LLMGATEWAY_CONFIG"
}
trap cleanup EXIT

start_gateway

# Unknown credentials are rejected without disclosing configured client secrets.
STATUS=$(curl -sS -o /tmp/client-invalid.json -w '%{http_code}'   -X POST http://127.0.0.1:7331/v1/chat/completions   -H "Authorization: Bearer invalid-client-key"   -H "Content-Type: application/json"   -d '{"model":"llmgateway-auto","messages":[{"role":"user","content":"hello"}]}')
[[ "$STATUS" == "401" ]]
grep -q '"authentication_error"' /tmp/client-invalid.json

# A prefer-browser client with explicit API fallback can use the only eligible API route.
curl -fsS -D /tmp/client-fallback.headers -o /tmp/client-fallback.json   -X POST http://127.0.0.1:7331/v1/chat/completions   -H "Authorization: Bearer $FALLBACK_CLIENT_KEY"   -H "Content-Type: application/json"   -d '{"model":"llmgateway-auto","messages":[{"role":"user","content":"fallback"}]}'
grep -qi '^x-llmgateway-route: api-route' /tmp/client-fallback.headers

# Browser-only is a hard boundary. The API route must never be used.
STATUS=$(curl -sS -o /tmp/client-browser-only.json -w '%{http_code}'   -X POST http://127.0.0.1:7331/v1/chat/completions   -H "Authorization: Bearer $BROWSER_ONLY_CLIENT_KEY"   -H "Content-Type: application/json"   -d '{"model":"llmgateway-auto","messages":[{"role":"user","content":"browser only"}]}')
[[ "$STATUS" == "400" ]]
grep -q '"model_error"' /tmp/client-browser-only.json

# A request-level override cannot broaden a restricted client's transport permissions.
STATUS=$(curl -sS -o /tmp/client-override.json -w '%{http_code}'   -X POST http://127.0.0.1:7331/v1/chat/completions   -H "Authorization: Bearer $BROWSER_ONLY_CLIENT_KEY"   -H "Content-Type: application/json"   -d '{"model":"llmgateway-auto","llmgateway_execution_preference":"api-only","messages":[{"role":"user","content":"try api"}]}')
[[ "$STATUS" == "403" ]]
grep -q '"client_policy_error"' /tmp/client-override.json

# Model allowlists affect both execution and /v1/models discovery.
curl -fsS -o /tmp/client-models.json   http://127.0.0.1:7331/v1/models   -H "Authorization: Bearer $MODEL_CLIENT_KEY"
python3 - <<'PY'
import json
x=json.load(open("/tmp/client-models.json"))
ids={item["id"] for item in x["data"]}
assert "allowed-vm" in ids, ids
assert "llmgateway-auto" not in ids, ids
assert "blocked-vm" not in ids, ids
PY

STATUS=$(curl -sS -o /tmp/client-model-denied.json -w '%{http_code}'   -X POST http://127.0.0.1:7331/v1/chat/completions   -H "Authorization: Bearer $MODEL_CLIENT_KEY"   -H "Content-Type: application/json"   -d '{"model":"blocked-vm","messages":[{"role":"user","content":"blocked"}]}')
[[ "$STATUS" == "403" ]]
grep -q '"client_policy_error"' /tmp/client-model-denied.json

# Route explain can diagnose client-scoped exclusions without exposing client keys.
EXPLAIN=$(curl -fsS -X POST http://127.0.0.1:7331/_llmgateway/routes/explain   -H "Authorization: Bearer $LLMGATEWAY_API_KEY"   -H "Content-Type: application/json"   -d '{"model":"llmgateway-auto","client_id":"browser-only"}')
printf '%s' "$EXPLAIN" | python3 -c '
import json,sys
x=json.load(sys.stdin)
assert x["selected_route"] is None, x
route=next(item for item in x["candidates"] if item["route_id"]=="api-route")
assert route["eligible"] is False, route
assert "policy_browser_only" in route["exclusion_reasons"], route
assert "client_policy_route_forbidden" in route["exclusion_reasons"], route
'

# Responses state is tenant-scoped. A different client must not be able to continue
# another client's previous_response_id, while the owning client can.
RESPONSE_A=$(curl -fsS -X POST http://127.0.0.1:7331/v1/responses   -H "Authorization: Bearer $RESPONSE_A_CLIENT_KEY"   -H "Content-Type: application/json"   -d '{"model":"llmgateway-auto","input":"response owner first"}')
RESPONSE_A_ID=$(printf '%s' "$RESPONSE_A" | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')

CROSS_OWNER_PAYLOAD=$(python3 -c 'import json,sys; print(json.dumps({"model":"llmgateway-auto","previous_response_id":sys.argv[1],"input":"steal context"}))' "$RESPONSE_A_ID")
STATUS=$(curl -sS -o /tmp/client-response-cross-owner.json -w '%{http_code}'   -X POST http://127.0.0.1:7331/v1/responses   -H "Authorization: Bearer $RESPONSE_B_CLIENT_KEY"   -H "Content-Type: application/json"   -d "$CROSS_OWNER_PAYLOAD")
[[ "$STATUS" == "404" ]]
grep -q '"invalid_request_error"' /tmp/client-response-cross-owner.json

OWNER_CONTINUE_PAYLOAD=$(python3 -c 'import json,sys; print(json.dumps({"model":"llmgateway-auto","previous_response_id":sys.argv[1],"input":"owner continues"}))' "$RESPONSE_A_ID")
RESPONSE_A_SECOND=$(curl -fsS -X POST http://127.0.0.1:7331/v1/responses   -H "Authorization: Bearer $RESPONSE_A_CLIENT_KEY"   -H "Content-Type: application/json"   -d "$OWNER_CONTINUE_PAYLOAD")
printf '%s' "$RESPONSE_A_SECOND" | grep -q 'fake reply messages=3'

# Request budget is consumed once and survives a gateway restart.
curl -fsS -o /tmp/client-budget-first.json   -X POST http://127.0.0.1:7331/v1/chat/completions   -H "Authorization: Bearer $REQUEST_BUDGET_CLIENT_KEY"   -H "Content-Type: application/json"   -d '{"model":"llmgateway-auto","messages":[{"role":"user","content":"first budget request"}]}'

stop_gateway
start_gateway

STATUS=$(curl -sS -o /tmp/client-budget-second.json -w '%{http_code}'   -X POST http://127.0.0.1:7331/v1/chat/completions   -H "Authorization: Bearer $REQUEST_BUDGET_CLIENT_KEY"   -H "Content-Type: application/json"   -d '{"model":"llmgateway-auto","messages":[{"role":"user","content":"second budget request"}]}')
[[ "$STATUS" == "429" ]]
grep -q '"client_budget_exceeded"' /tmp/client-budget-second.json

# Token-budgeted requests without an output cap receive a bounded max_tokens reservation.
# Provider-reported usage then reconciles the reservation before restart persistence is checked.
curl -fsS -o /tmp/client-token-first.json   -X POST http://127.0.0.1:7331/v1/chat/completions   -H "Authorization: Bearer $TOKEN_BUDGET_CLIENT_KEY"   -H "Content-Type: application/json"   -d '{"model":"llmgateway-auto","messages":[{"role":"user","content":"small token request without cap"}]}'

python3 - <<'PY'
import sqlite3
db=sqlite3.connect("data/llmgateway-client-policies.db")
row=db.execute("""
select input_tokens, reserved_output_tokens, observed_input_tokens, observed_output_tokens
from client_usage_events where client_id='token-budget'
order by occurred_at desc limit 1
""").fetchone()
assert row is not None, row
assert row[1] > 0, row
assert row[2] == 1, row
assert row[3] == 3, row
PY

stop_gateway
start_gateway

STATUS=$(curl -sS -o /tmp/client-token-second.json -w '%{http_code}'   -X POST http://127.0.0.1:7331/v1/chat/completions   -H "Authorization: Bearer $TOKEN_BUDGET_CLIENT_KEY"   -H "Content-Type: application/json"   -d '{"model":"llmgateway-auto","max_tokens":500,"messages":[{"role":"user","content":"large token request"}]}')
[[ "$STATUS" == "429" ]]
grep -q '"client_budget_exceeded"' /tmp/client-token-second.json

# Admin diagnostics expose policy/budget state, never raw client key values.
curl -fsS -o /tmp/client-diagnostics.json   http://127.0.0.1:7331/_llmgateway/clients   -H "Authorization: Bearer $LLMGATEWAY_API_KEY"
python3 - <<'PY'
import json
x=json.load(open("/tmp/client-diagnostics.json"))
assert x["secrets_exposed"] is False, x
clients={item["id"]:item for item in x["data"]}
assert clients["request-budget"]["budget"]["daily"]["requests"] == 1, clients["request-budget"]
assert clients["request-budget"]["budget"]["daily"]["request_remaining"] == 0, clients["request-budget"]
assert clients["token-budget"]["budget"]["daily"]["requests"] == 1, clients["token-budget"]
assert clients["token-budget"]["budget"]["daily"]["token_remaining"] is not None, clients["token-budget"]
assert clients["token-budget"]["budget"]["daily"]["token_remaining"] < 500, clients["token-budget"]
assert all(item["key_configured"] for item in clients.values()), clients
PY
for secret in   "$FALLBACK_CLIENT_KEY" "$BROWSER_ONLY_CLIENT_KEY" "$REQUEST_BUDGET_CLIENT_KEY"   "$TOKEN_BUDGET_CLIENT_KEY" "$MODEL_CLIENT_KEY" "$RESPONSE_A_CLIENT_KEY" "$RESPONSE_B_CLIENT_KEY"; do
  if grep -Fq "$secret" /tmp/client-diagnostics.json; then
    echo "client secret leaked through diagnostics" >&2
    exit 1
  fi
done

# Legacy deployments with no [clients.*] keep the original global API-key behavior.
stop_gateway
write_legacy_config
start_gateway
curl -fsS -o /tmp/client-legacy.json   -X POST http://127.0.0.1:7331/v1/chat/completions   -H "Authorization: Bearer $LLMGATEWAY_API_KEY"   -H "Content-Type: application/json"   -d '{"model":"llmgateway-auto","messages":[{"role":"user","content":"legacy global key"}]}'

echo "llmgateway client policies and budgets smoke test passed"
