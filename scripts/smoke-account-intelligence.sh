#!/usr/bin/env bash
set -euo pipefail

export LLMGATEWAY_API_KEY="ci-local-key"
export FAKE_API_KEY="healthy"
export LLMGATEWAY_CONFIG="/tmp/llmgateway-account-intelligence.toml"
PROFILE_ROOT="/tmp/llmgateway-account-intelligence-profiles"

rm -rf "$PROFILE_ROOT"
rm -f data/llmgateway.db data/llmgateway.db-shm data/llmgateway.db-wal
mkdir -p data

cat >"$LLMGATEWAY_CONFIG" <<EOF
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

[browser]
enabled = true
profile_root = "$PROFILE_ROOT"

[browser.sessions.fake-web]
provider = "fake-web"
label = "Fake Browser"
login_url = "http://127.0.0.1:18082/login"
enabled = true

[browser.bindings.browser-account]
session = "fake-web"

[[providers]]
id = "fake-web"
kind = "browser-http"
base_url = "http://127.0.0.1:18082/v1"

[[providers]]
id = "fake-api"
kind = "openai-compatible"
base_url = "http://127.0.0.1:18080/v1"

[[accounts]]
id = "browser-account"
provider = "fake-web"
enabled = true

[[accounts]]
id = "api-account"
provider = "fake-api"
api_key_env = "FAKE_API_KEY"
enabled = true
discover_models = false

[[routes]]
id = "browser-route"
account = "browser-account"
model = "browser-model"
priority = 1
enabled = true
capabilities = ["chat"]

[[routes]]
id = "api-route"
account = "api-account"
model = "fake-model"
priority = 10
enabled = true
capabilities = ["chat"]

[virtual_models.llmgateway-auto]
routes = ["browser-route", "api-route"]
EOF

python3 scripts/fake-openai.py >/tmp/llmgateway-account-intelligence-fake.log 2>&1 &
FAKE_PID=$!

cargo build --quiet
./target/debug/llmgateway >/tmp/llmgateway-account-intelligence.log 2>&1 &
PID=$!
cleanup() {
  kill "$PID" "$FAKE_PID" 2>/dev/null || true
  rm -rf "$PROFILE_ROOT"
  rm -f data/llmgateway.db data/llmgateway.db-shm data/llmgateway.db-wal
  rm -f "$LLMGATEWAY_CONFIG"
}
trap cleanup EXIT

for _ in {1..60}; do
  if curl -fsS http://127.0.0.1:7331/_llmgateway/health >/dev/null; then
    break
  fi
  sleep 0.2
done

INTELLIGENCE=$(curl -fsS \
  -H "Authorization: Bearer ${LLMGATEWAY_API_KEY}" \
  http://127.0.0.1:7331/_llmgateway/account-intelligence)

printf '%s' "$INTELLIGENCE" | python3 -c '
import json,sys
payload=json.load(sys.stdin)
accounts={item["id"]: item for item in payload.get("data", [])}

browser=accounts["browser-account"]
assert browser["provider"] == "fake-web", browser
assert browser["provider_kind"] == "browser-http", browser
assert browser["transport"] == "browser", browser
assert browser["credential_required"] is False, browser
assert browser["credential_configured"] is None, browser
assert browser["discover_models"] is False, browser
assert browser["route_ids"] == ["browser-route"], browser
assert browser["routing_state"] == "requires_login", browser
assert browser["browser_session"]["id"] == "fake-web", browser
assert browser["browser_session"]["label"] == "Fake Browser", browser

api=accounts["api-account"]
assert api["provider"] == "fake-api", api
assert api["provider_kind"] == "openai-compatible", api
assert api["transport"] == "api", api
assert api["credential_required"] is True, api
assert api["credential_configured"] is True, api
assert api["discover_models"] is False, api
assert api["route_ids"] == ["api-route"], api
assert api["routing_state"] == "ready", api
assert api["browser_session"] is None, api
'

echo "llmgateway account intelligence smoke test passed"
