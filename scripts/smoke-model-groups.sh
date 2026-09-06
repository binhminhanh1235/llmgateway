#!/usr/bin/env bash
set -euo pipefail

export LLMGATEWAY_API_KEY="ci-local-key"
export GROUP_ROUTE_KEY="healthy"
export LLMGATEWAY_CONFIG="/tmp/llmgateway-model-groups.toml"

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

[context]
enabled = false
retrieval_enabled = false

[[providers]]
id = "fake"
kind = "openai-compatible"
base_url = "http://127.0.0.1:18080/v1"

[[accounts]]
id = "group-account"
provider = "fake"
api_key_env = "GROUP_ROUTE_KEY"
enabled = true
discover_models = false

[[routes]]
id = "slow-primary"
account = "group-account"
model = "model-primary"
priority = 100
enabled = true
capabilities = ["chat"]

[[routes]]
id = "fast-fallback"
account = "group-account"
model = "model-fallback"
priority = 1
enabled = true
capabilities = ["chat"]

[virtual_models.llmgateway-auto]
routes = ["slow-primary", "fast-fallback"]
EOF

cargo build --quiet
./target/debug/llmgateway >/tmp/llmgateway-model-groups.log 2>&1 &
PID=$!
cleanup() {
  kill "$PID" 2>/dev/null || true
  rm -f data/llmgateway.db data/llmgateway.db-shm data/llmgateway.db-wal "$LLMGATEWAY_CONFIG"
}
trap cleanup EXIT

for _ in {1..60}; do
  curl -fsS http://127.0.0.1:7331/_llmgateway/health >/dev/null && break
  sleep 0.2
done

AUTH=(-H "Authorization: Bearer ${LLMGATEWAY_API_KEY}")
JSON=(-H "Content-Type: application/json")

GROUP_PAYLOAD=$(curl -fsS http://127.0.0.1:7331/_llmgateway/model-groups "${AUTH[@]}")
printf '%s' "$GROUP_PAYLOAD" | python3 -c '
import json,sys
x=json.load(sys.stdin)
ids={m["id"] for m in x["models"]}
assert "fake/model-primary" in ids, x
assert "fake/model-fallback" in ids, x
assert all("route" not in m["display_name"].lower() for m in x["models"]), x
'

CREATE=$(curl -fsS -X POST http://127.0.0.1:7331/_llmgateway/model-groups "${AUTH[@]}" "${JSON[@]}" \
  -d '{"id":"ci-tiered","tiers":[{"priority":10,"models":["fake/model-primary","fake/model-fallback"]}]}')
printf '%s' "$CREATE" | python3 -c '
import json,sys
x=json.load(sys.stdin)
assert x["group"]["id"] == "ci-tiered", x
assert x["group"]["mode"] == "model-tiered", x
assert x["group"]["tiers"][0]["models"] == ["fake/model-primary","fake/model-fallback"], x
assert x["restart_required"] is False, x
'

MODELS=$(curl -fsS http://127.0.0.1:7331/v1/models "${AUTH[@]}")
printf '%s' "$MODELS" | python3 -c '
import json,sys
x=json.load(sys.stdin)
assert "ci-tiered" in {m["id"] for m in x["data"]}, x
'

EXPLAIN=$(curl -fsS -X POST http://127.0.0.1:7331/_llmgateway/routes/explain "${AUTH[@]}" "${JSON[@]}" \
  -d '{"model":"ci-tiered"}')
printf '%s' "$EXPLAIN" | python3 -c '
import json,sys
x=json.load(sys.stdin)
c={r["route_id"]:r for r in x["candidates"]}
assert x["selected_route"] == "slow-primary", x
assert c["slow-primary"]["group_tier_priority"] == 10, c
assert c["fast-fallback"]["group_tier_priority"] == 10, c
assert c["slow-primary"]["group_model_order"] == 0, c
assert c["fast-fallback"]["group_model_order"] == 1, c
assert c["slow-primary"]["final_score"] > c["fast-fallback"]["final_score"], c
'

curl -fsS -X PUT http://127.0.0.1:7331/_llmgateway/model-groups/ci-tiered "${AUTH[@]}" "${JSON[@]}" \
  -d '{"tiers":[{"priority":10,"models":["fake/model-fallback","fake/model-primary"]}]}' >/tmp/model-group-update.json

EXPLAIN=$(curl -fsS -X POST http://127.0.0.1:7331/_llmgateway/routes/explain "${AUTH[@]}" "${JSON[@]}" \
  -d '{"model":"ci-tiered"}')
printf '%s' "$EXPLAIN" | python3 -c '
import json,sys
x=json.load(sys.stdin)
assert x["selected_route"] == "fast-fallback", x
c={r["route_id"]:r for r in x["candidates"]}
assert c["fast-fallback"]["group_model_order"] == 0, c
assert c["slow-primary"]["group_model_order"] == 1, c
'

curl -fsS -X DELETE http://127.0.0.1:7331/_llmgateway/model-groups/ci-tiered "${AUTH[@]}" \
  | python3 -c 'import json,sys; x=json.load(sys.stdin); assert x["deleted"] is True, x'

MODELS=$(curl -fsS http://127.0.0.1:7331/v1/models "${AUTH[@]}")
printf '%s' "$MODELS" | python3 -c '
import json,sys
x=json.load(sys.stdin)
assert "ci-tiered" not in {m["id"] for m in x["data"]}, x
'

echo "llmgateway active-model group CRUD + ordered fallback smoke test passed"
