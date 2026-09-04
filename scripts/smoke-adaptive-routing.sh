#!/usr/bin/env bash
set -euo pipefail

export LLMGATEWAY_API_KEY="ci-adaptive-key"
export FAKE_PRIMARY_KEY="slow200"
export FAKE_SECONDARY_KEY="healthy"
export LLMGATEWAY_CONFIG="/tmp/llmgateway-adaptive-smoke.toml"

rm -f data/llmgateway.db data/llmgateway.db-shm data/llmgateway.db-wal
mkdir -p data

cat >/tmp/llmgateway-adaptive-smoke.toml <<'EOF'
[server]
host = "127.0.0.1"
port = 7331

[api]
key_env = "LLMGATEWAY_API_KEY"
default_model = "llmgateway-auto"

[storage]
database_url = "sqlite://data/llmgateway.db"

[context]
enabled = false
retrieval_enabled = false

[routing]
adaptive_enabled = true
adaptive_min_samples = 2
adaptive_history_samples = 10
adaptive_stale_after_seconds = 1
adaptive_ewma_alpha = 1.0
adaptive_latency_target_ms = 50
adaptive_max_penalty = 30
adaptive_failure_weight = 0.0

[usage]
enabled = true
hard_limits = true
balance_weight = 0

[[providers]]
id = "fake"
kind = "openai-compatible"
base_url = "http://127.0.0.1:18080/v1"
models_path = "models"

[[accounts]]
id = "slow-account"
provider = "fake"
api_key_env = "FAKE_PRIMARY_KEY"
auth_style = "bearer"
enabled = true
discover_models = false

[[accounts]]
id = "fast-account"
provider = "fake"
api_key_env = "FAKE_SECONDARY_KEY"
auth_style = "bearer"
enabled = true
discover_models = false

[[routes]]
id = "slow-route"
account = "slow-account"
model = "fake-model"
priority = 10
enabled = true
capabilities = ["chat"]

[[routes]]
id = "fast-route"
account = "fast-account"
model = "fake-model"
priority = 10
enabled = true
capabilities = ["chat"]

[virtual_models.llmgateway-auto]
routes = ["slow-route", "fast-route"]
EOF

python3 scripts/fake-openai.py >/tmp/llmgateway-adaptive-fake.log 2>&1 &
FAKE_PID=$!
cargo build --quiet
./target/debug/llmgateway >/tmp/llmgateway-adaptive.log 2>&1 &
PID=$!
trap 'kill "$PID" "$FAKE_PID" 2>/dev/null || true; rm -f data/llmgateway.db data/llmgateway.db-shm data/llmgateway.db-wal /tmp/llmgateway-adaptive-smoke.toml /tmp/adaptive.headers /tmp/adaptive.json' EXIT

wait_ready() {
  for _ in {1..60}; do
    if curl -fsS http://127.0.0.1:7331/_llmgateway/health >/dev/null; then
      return 0
    fi
    sleep 0.2
  done
  echo "gateway did not become ready" >&2
  return 1
}

wait_ready

request() {
  curl -fsS -D /tmp/adaptive.headers -o /tmp/adaptive.json     -X POST http://127.0.0.1:7331/v1/chat/completions     -H "Authorization: Bearer ${LLMGATEWAY_API_KEY}"     -H "Content-Type: application/json"     -d '{"model":"llmgateway-auto","stream":false,"messages":[{"role":"user","content":"adaptive smoke"}]}'
  awk 'BEGIN{IGNORECASE=1} /^x-llmgateway-route:/ {gsub("\r", "", $2); print $2}' /tmp/adaptive.headers
}

FIRST=$(request)
SECOND=$(request)
test "$FIRST" = "slow-route"
test "$SECOND" = "slow-route"

# Restart against the same SQLite database. The new process must recover adaptive
# samples from execution_attempts before it starts serving traffic.
kill "$PID"
wait "$PID" 2>/dev/null || true
./target/debug/llmgateway >/tmp/llmgateway-adaptive-restart.log 2>&1 &
PID=$!
wait_ready
grep -q 'restored adaptive route samples from execution trace' /tmp/llmgateway-adaptive-restart.log

EXPLAIN=$(curl -fsS   -X POST http://127.0.0.1:7331/_llmgateway/routes/explain   -H "Authorization: Bearer ${LLMGATEWAY_API_KEY}"   -H "Content-Type: application/json"   -d '{"model":"llmgateway-auto"}')

printf '%s' "$EXPLAIN" | python3 -c '
import json,sys
x=json.load(sys.stdin)
assert x["selected_route"] == "fast-route", x
by_id={c["route_id"]: c for c in x["candidates"]}
slow=by_id["slow-route"]
fast=by_id["fast-route"]
assert slow["eligible"] is True, slow
assert slow["adaptive"]["active"] is True, slow
assert slow["adaptive"]["sample_count"] == 2, slow
assert slow["adaptive"]["success_rate"] == 1.0, slow
assert slow["adaptive"]["ewma_latency_ms"] >= 100, slow
assert slow["adaptive_penalty"] > 0, slow
assert slow["final_score"] > fast["final_score"], (slow, fast)
assert fast["adaptive"]["sample_count"] == 0, fast
assert fast["adaptive_penalty"] == 0, fast
'

THIRD=$(request)
test "$THIRD" = "fast-route"

# Once telemetry becomes stale, the penalty must become neutral so the original
# priority order gets a deterministic recovery probe.
sleep 2
RECOVERY_EXPLAIN=$(curl -fsS \
  -X POST http://127.0.0.1:7331/_llmgateway/routes/explain \
  -H "Authorization: Bearer ${LLMGATEWAY_API_KEY}" \
  -H "Content-Type: application/json" \
  -d '{"model":"llmgateway-auto"}')
printf '%s' "$RECOVERY_EXPLAIN" | python3 -c '
import json,sys
x=json.load(sys.stdin)
assert x["selected_route"] == "slow-route", x
by_id={c["route_id"]: c for c in x["candidates"]}
slow=by_id["slow-route"]
assert slow["adaptive"]["sample_count"] == 2, slow
assert slow["adaptive"]["stale"] is True, slow
assert slow["adaptive"]["active"] is False, slow
assert slow["adaptive_penalty"] == 0, slow
'

FOURTH=$(request)
test "$FOURTH" = "slow-route"

echo "llmgateway adaptive recovery smoke test passed"
