#!/usr/bin/env bash
set -euo pipefail

export LLMGATEWAY_API_KEY="ci-local-key"
export FAKE_API_KEY="healthy"
export LLMGATEWAY_CONFIG="/tmp/llmgateway-first-class-browser.toml"
PROFILE_ROOT="/tmp/llmgateway-first-class-browser-profiles"

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
login_url = "http://127.0.0.1:18082/login"
enabled = true

[browser.bindings.browser-account]
session = "fake-web"

[[providers]]
id = "fake-web"
kind = "browser-http"
base_url = "http://127.0.0.1:18082/v1"
models_path = "models"

[[providers]]
id = "fake-api"
kind = "openai-compatible"
base_url = "http://127.0.0.1:18080/v1"
models_path = "models"

# Intentionally no api_key_env, auth_style, or discover_models.
[[accounts]]
id = "browser-account"
provider = "fake-web"
enabled = true

[[accounts]]
id = "api-account"
provider = "fake-api"
api_key_env = "FAKE_API_KEY"
auth_style = "bearer"
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

python3 scripts/fake-openai.py >/tmp/llmgateway-first-class-browser-fake.log 2>&1 &
FAKE_PID=$!

cargo build --quiet
./target/debug/llmgateway >/tmp/llmgateway-first-class-browser.log 2>&1 &
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

AUTH=(-H "Authorization: Bearer ${LLMGATEWAY_API_KEY}")
JSON=(-H "Content-Type: application/json")

curl -fsS -D /tmp/first-class-browser.headers -o /tmp/first-class-browser.json \
  -X POST http://127.0.0.1:7331/v1/chat/completions \
  "${AUTH[@]}" "${JSON[@]}" \
  -d '{"model":"llmgateway-auto","stream":false,"messages":[{"role":"user","content":"browser account should not need dummy credentials"}]}'

grep -qi '^x-llmgateway-route: api-route' /tmp/first-class-browser.headers

ACCOUNTS=$(curl -fsS http://127.0.0.1:7331/_llmgateway/accounts "${AUTH[@]}")
printf '%s' "$ACCOUNTS" | python3 -c '
import json,sys
x=json.load(sys.stdin)
items=x if isinstance(x,list) else x.get("accounts",[])
account=next(a for a in items if a.get("id") == "browser-account")
assert account.get("discover_models") is False, account
'

echo "llmgateway first-class browser account smoke test passed"
