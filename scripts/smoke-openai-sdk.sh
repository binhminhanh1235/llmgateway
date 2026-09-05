#!/usr/bin/env bash
set -euo pipefail

export LLMGATEWAY_API_KEY="ci-local-key"
export FAKE_API_KEY="fake-key"
export LLMGATEWAY_CONFIG="/tmp/llmgateway-openai-sdk-smoke.toml"
SDK_DIR="/tmp/llmgateway-openai-sdk"

rm -rf "$SDK_DIR"
rm -f data/llmgateway.db data/llmgateway.db-shm data/llmgateway.db-wal
mkdir -p data "$SDK_DIR"

cat >"$LLMGATEWAY_CONFIG" <<'EOF'
[server]
host = "127.0.0.1"
port = 7331

[api]
key_env = "LLMGATEWAY_API_KEY"
default_model = "llmgateway-auto"
strict_openai_compatibility = true

[storage]
database_url = "sqlite://data/llmgateway.db"

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
api_key_env = "FAKE_API_KEY"
auth_style = "bearer"
enabled = true
discover_models = false

[[routes]]
id = "fake-route"
account = "fake-primary"
model = "fake-model"
priority = 10
enabled = true
capabilities = ["chat"]

[virtual_models.llmgateway-auto]
routes = ["fake-route"]

[virtual_models.llmgateway-coding]
routes = ["fake-route"]

[virtual_models.llmgateway-best]
routes = ["fake-route"]
EOF

python3 scripts/fake-openai.py >/tmp/llmgateway-openai-sdk-fake.log 2>&1 &
FAKE_PID=$!
cargo build --quiet
./target/debug/llmgateway >/tmp/llmgateway-openai-sdk.log 2>&1 &
PID=$!

cleanup() {
  kill "$PID" "$FAKE_PID" 2>/dev/null || true
  rm -rf "$SDK_DIR"
  rm -f "$LLMGATEWAY_CONFIG"
  rm -f data/llmgateway.db data/llmgateway.db-shm data/llmgateway.db-wal
}
trap cleanup EXIT

for _ in {1..40}; do
  if curl -fsS http://127.0.0.1:7331/_llmgateway/health >/dev/null; then
    break
  fi
  sleep 0.2
done

ERROR_BODY=/tmp/llmgateway-openai-sdk-error.json
for payload in \
  '{"messages":[{"role":"user","content":"missing model"}]}' \
  '{"model":"","messages":[{"role":"user","content":"blank model"}]}' \
  '{"model":123,"messages":[{"role":"user","content":"non-string model"}]}'; do
  STATUS=$(curl -sS -o "$ERROR_BODY" -w '%{http_code}' -X POST \
    http://127.0.0.1:7331/v1/chat/completions \
    -H "Authorization: Bearer ${LLMGATEWAY_API_KEY}" \
    -H "Content-Type: application/json" \
    -d "$payload")
  test "$STATUS" = "400"
  python3 - "$ERROR_BODY" <<'PY'
import json,sys
with open(sys.argv[1], encoding="utf-8") as f:
    payload=json.load(f)
assert payload["error"]["type"] == "invalid_request_error", payload
assert "'model' is required" in payload["error"]["message"], payload
PY
done

python3 -m pip install --disable-pip-version-check --quiet --target "$SDK_DIR" 'openai>=1,<3'

PYTHONPATH="$SDK_DIR" python3 - <<'PY'
from openai import OpenAI

client = OpenAI(
    api_key="ci-local-key",
    base_url="http://127.0.0.1:7331/v1",
)
completion = client.chat.completions.create(
    model="llmgateway-auto",
    messages=[{"role": "user", "content": "official sdk compatibility"}],
)
assert completion.model == "fake-model", completion
assert completion.choices[0].message.content == "fake reply messages=1", completion
PY

echo "llmgateway strict OpenAI Chat Completions compatibility smoke test passed"
