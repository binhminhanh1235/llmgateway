#!/usr/bin/env bash
set -euo pipefail

PORT=17331
export LLMGATEWAY_API_KEY="ci-chatgpt-all-key"
export LLMGATEWAY_CONFIG="/tmp/llmgateway-chatgpt-all.toml"
export LLMGATEWAY_BASE_URL="http://127.0.0.1:${PORT}"
export CHATGPT_ACCOUNT_ID="chatgpt-web-41d15799"
export CHATGPT_SESSION_ID="chatgpt-web-41d15799"
export CHATGPT_MODEL_ID="chatgpt-web/chatgpt-web-default"

PROFILE_ROOT="/tmp/llmgateway-chatgpt-all-profiles"
FAKE_CHROMIUM="/tmp/llmgateway-fake-chatgpt-all-chromium"
TEST_DB="/tmp/llmgateway-chatgpt-all.db"
BROWSER_PID=""
GATEWAY_PID=""

rm -rf "$PROFILE_ROOT" "$TEST_DB" "${TEST_DB}-shm" "${TEST_DB}-wal"
mkdir -p "$PROFILE_ROOT"

cat >"$FAKE_CHROMIUM" <<'SH'
#!/usr/bin/env bash
exec python3 scripts/fake-cdp-chromium.py "$@"
SH
chmod 700 "$FAKE_CHROMIUM"

cat >"$LLMGATEWAY_CONFIG" <<EOF
[server]
host = "127.0.0.1"
port = ${PORT}

[api]
key_env = "LLMGATEWAY_API_KEY"
default_model = "llmgateway-auto"

[storage]
database_url = "sqlite://${TEST_DB}"

[browser]
enabled = true
profile_root = "${PROFILE_ROOT}"

[browser.sessions.${CHATGPT_SESSION_ID}]
provider = "chatgpt-web"
label = "${CHATGPT_SESSION_ID}"
login_url = "https://chatgpt.com/"
enabled = true

[browser.bindings.${CHATGPT_ACCOUNT_ID}]
session = "${CHATGPT_SESSION_ID}"
adapter_contract_version = 1
models = ["chatgpt-web-default"]
ephemeral_chat = true
probe_timeout_ms = 5000
response_timeout_ms = 30000
first_byte_timeout_ms = 15000
idle_stream_timeout_ms = 15000

[chromium]
enabled = true
executable = "${FAKE_CHROMIUM}"
startup_timeout_seconds = 5
auto_recover = true
reconcile_interval_seconds = 15
extra_args = []

[chromium.sessions.${CHATGPT_SESSION_ID}]
enabled = true
ready_url_prefixes = ["https://chatgpt.com/"]

[context]
enabled = true
target_tokens = 16000
reserve_output_tokens = 4000
recent_messages = 12
compaction_trigger_ratio = 0.85
summary_input_tokens = 12000
summary_max_tokens = 1200
retrieval_enabled = false

[[providers]]
id = "chatgpt-web"
kind = "browser-chatgpt"

[[accounts]]
id = "${CHATGPT_ACCOUNT_ID}"
provider = "chatgpt-web"
enabled = true
discover_models = false

[[routes]]
id = "chatgpt-web-41d15799-route"
account = "${CHATGPT_ACCOUNT_ID}"
model = "chatgpt-web-default"
priority = 1
enabled = true
capabilities = ["chat", "coding", "reasoning"]

[virtual_models.llmgateway-auto]
routes = ["chatgpt-web-41d15799-route"]

[virtual_models.llmgateway-best]
routes = ["chatgpt-web-41d15799-route"]
EOF

cleanup() {
  if [ -n "$BROWSER_PID" ]; then kill "$BROWSER_PID" 2>/dev/null || true; wait "$BROWSER_PID" 2>/dev/null || true; fi
  if [ -n "$GATEWAY_PID" ]; then kill "$GATEWAY_PID" 2>/dev/null || true; wait "$GATEWAY_PID" 2>/dev/null || true; fi
  rm -rf "$PROFILE_ROOT"
  rm -f "$FAKE_CHROMIUM" "$LLMGATEWAY_CONFIG" "$TEST_DB" "${TEST_DB}-shm" "${TEST_DB}-wal"
}
trap cleanup EXIT

echo "Starting llmgateway on port ${PORT}..."
./target/release/llmgateway >/tmp/llmgateway-chatgpt-all.log 2>&1 &
GATEWAY_PID=$!

for _ in {1..60}; do
  curl -fsS "http://127.0.0.1:${PORT}/_llmgateway/health" >/dev/null 2>&1 && break
  sleep 0.2
done

AUTH=(-H "Authorization: Bearer ${LLMGATEWAY_API_KEY}")

echo "Launching Chromium driver for ${CHATGPT_SESSION_ID}..."
LAUNCH=$(curl -fsS -X POST \
  "http://127.0.0.1:${PORT}/_llmgateway/browser-sessions/${CHATGPT_SESSION_ID}/driver/launch" \
  "${AUTH[@]}")
BROWSER_PID=$(printf '%s' "$LAUNCH" | python3 -c 'import json,sys; print(json.load(sys.stdin)["launch"]["pid"] or "")')

echo "Verifying session authentication..."
curl -fsS -X POST \
  "http://127.0.0.1:${PORT}/_llmgateway/browser-sessions/${CHATGPT_SESSION_ID}/driver/verify" \
  "${AUTH[@]}" >/dev/null

echo "Running comprehensive ChatGPT API test suite..."
python3 scripts/test-all-chatgpt-apis.py

echo "All ChatGPT API tests completed successfully!"
