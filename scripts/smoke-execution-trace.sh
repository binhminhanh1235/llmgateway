#!/usr/bin/env bash
set -euo pipefail

export LLMGATEWAY_API_KEY="ci-execution-key"
export FAKE_PRIMARY_KEY="quota429"
export FAKE_SECONDARY_KEY="healthy"
export LLMGATEWAY_CONFIG="/tmp/llmgateway-execution-smoke.toml"

rm -f data/llmgateway.db data/llmgateway.db-shm data/llmgateway.db-wal
mkdir -p data

cat >/tmp/llmgateway-execution-smoke.toml <<'EOF'
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
EOF

python3 scripts/fake-openai.py >/tmp/llmgateway-execution-fake.log 2>&1 &
FAKE_PID=$!
cargo build --quiet
./target/debug/llmgateway >/tmp/llmgateway-execution.log 2>&1 &
PID=$!
trap 'kill "$PID" "$FAKE_PID" 2>/dev/null || true; rm -f data/llmgateway.db data/llmgateway.db-shm data/llmgateway.db-wal /tmp/llmgateway-execution-smoke.toml /tmp/execution.headers /tmp/execution.json /tmp/execution-failure.headers /tmp/execution-failure.json' EXIT

for _ in {1..60}; do
  if curl -fsS http://127.0.0.1:7331/_llmgateway/health >/dev/null; then
    break
  fi
  sleep 0.2
done

# v0.22 embeds a trace console wired directly to the v0.21 execution APIs.
UI_HTML=$(curl -fsS http://127.0.0.1:7331/)
TRACE_JS=$(curl -fsS http://127.0.0.1:7331/ui/trace-console.js)
TRACE_CSS=$(curl -fsS http://127.0.0.1:7331/ui/trace-console.css)
grep -q 'data-view="traces"' <<<"$UI_HTML"
grep -q 'id="tracesView"' <<<"$UI_HTML"
grep -q '/ui/trace-console.js' <<<"$UI_HTML"
grep -q '/ui/trace-console.css' <<<"$UI_HTML"
grep -q '/_llmgateway/executions?limit=100' <<<"$TRACE_JS"
grep -q '/_llmgateway/executions/' <<<"$TRACE_JS"
grep -q 'trace-console-shell' <<<"$TRACE_CSS"
grep -q 'trace-timeline' <<<"$TRACE_CSS"

curl -fsS -D /tmp/execution.headers -o /tmp/execution.json \
  -X POST http://127.0.0.1:7331/v1/chat/completions \
  -H "Authorization: Bearer ${LLMGATEWAY_API_KEY}" \
  -H "Content-Type: application/json" \
  -d '{"model":"llmgateway-auto","stream":false,"messages":[{"role":"user","content":"execution-secret-prompt"}]}'

grep -qi '^x-llmgateway-route: secondary-route' /tmp/execution.headers
REQUEST_ID=$(awk 'BEGIN{IGNORECASE=1} /^x-llmgateway-request-id:/ {gsub("\r", "", $2); print $2}' /tmp/execution.headers)
test -n "$REQUEST_ID"
case "$REQUEST_ID" in
  req_*) ;;
  *) echo "unexpected request id: $REQUEST_ID" >&2; exit 1 ;;
esac

TRACE=$(curl -fsS "http://127.0.0.1:7331/_llmgateway/executions/${REQUEST_ID}" \
  -H "Authorization: Bearer ${LLMGATEWAY_API_KEY}")
printf '%s' "$TRACE" | python3 -c '
import json,sys
x=json.load(sys.stdin)
assert x["request_id"].startswith("req_"), x
assert x["requested_model"] == "llmgateway-auto", x
assert x["status"] == "success", x
assert x["selected_route"] == "secondary-route", x
assert x["attempt_count"] == 2, x
assert x["final_error"] is None, x
attempts=x["attempts"]
assert len(attempts) == 2, attempts
first, second = attempts
assert first["attempt_index"] == 0, first
assert first["route_id"] == "primary-route", first
assert first["account_id"] == "fake-primary", first
assert first["status_code"] == 429, first
assert first["outcome"] == "rate_limited", first
assert first["retryable"] is True, first
assert first["duration_ms"] >= 0, first
assert second["attempt_index"] == 1, second
assert second["route_id"] == "secondary-route", second
assert second["account_id"] == "fake-secondary", second
assert second["status_code"] == 200, second
assert second["outcome"] == "success", second
assert second["retryable"] is False, second
'
if grep -q 'execution-secret-prompt' <<<"$TRACE"; then
  echo "execution trace leaked prompt content" >&2
  exit 1
fi

RECENT=$(curl -fsS "http://127.0.0.1:7331/_llmgateway/executions?limit=10" \
  -H "Authorization: Bearer ${LLMGATEWAY_API_KEY}")
printf '%s' "$RECENT" | python3 -c "
import json,sys
x=json.load(sys.stdin)
request_id='${REQUEST_ID}'
items={item['request_id']: item for item in x['data']}
assert request_id in items, x
assert items[request_id]['status'] == 'success', items[request_id]
assert items[request_id]['attempt_count'] == 2, items[request_id]
"

# A request that cannot be routed must still expose a correlation ID and a failed trace.
HTTP_CODE=$(curl -sS -D /tmp/execution-failure.headers -o /tmp/execution-failure.json \
  -w '%{http_code}' \
  -X POST http://127.0.0.1:7331/v1/chat/completions \
  -H "Authorization: Bearer ${LLMGATEWAY_API_KEY}" \
  -H "Content-Type: application/json" \
  -d '{"model":"does-not-exist","stream":false,"messages":[{"role":"user","content":"unroutable"}]}')
test "$HTTP_CODE" = "400"
FAILED_REQUEST_ID=$(awk 'BEGIN{IGNORECASE=1} /^x-llmgateway-request-id:/ {gsub("\r", "", $2); print $2}' /tmp/execution-failure.headers)
test -n "$FAILED_REQUEST_ID"
FAILED_TRACE=$(curl -fsS "http://127.0.0.1:7331/_llmgateway/executions/${FAILED_REQUEST_ID}" \
  -H "Authorization: Bearer ${LLMGATEWAY_API_KEY}")
printf '%s' "$FAILED_TRACE" | python3 -c '
import json,sys
x=json.load(sys.stdin)
assert x["requested_model"] == "does-not-exist", x
assert x["status"] == "failed", x
assert x["selected_route"] is None, x
assert x["attempt_count"] == 0, x
assert x["final_error"], x
assert x["attempts"] == [], x
'

echo "llmgateway execution trace + trace console smoke test passed"
