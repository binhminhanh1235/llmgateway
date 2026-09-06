#!/usr/bin/env bash
set -euo pipefail

export LLMGATEWAY_API_KEY="ci-native-agent-key"
export LLMGATEWAY_CLIENT_API_KEY="$LLMGATEWAY_API_KEY"
export FAKE_API_KEY="healthy"
export LLMGATEWAY_BASE_URL="http://127.0.0.1:7331"
export LLMGATEWAY_CONFIG="/tmp/llmgateway-native-agent-mcp.toml"

DB="data/llmgateway-native-agent-mcp.db"
FAKE_BIN="/tmp/llmgateway-native-browser-bin"
rm -f "$DB" "$DB-shm" "$DB-wal"
rm -rf "$FAKE_BIN"
mkdir -p data "$FAKE_BIN"

cat >"$FAKE_BIN/google-chrome" <<'SH'
#!/usr/bin/env bash
exit 0
SH
chmod +x "$FAKE_BIN/google-chrome"
export PATH="$FAKE_BIN:$PATH"

cat >"$LLMGATEWAY_CONFIG" <<'EOF'
[server]
host = "127.0.0.1"
port = 7331

[api]
key_env = "LLMGATEWAY_API_KEY"
default_model = "llmgateway-auto"

[storage]
database_url = "sqlite://data/llmgateway-native-agent-mcp.db"

[routing]
adaptive_enabled = false
task_aware_enabled = false
execution_preference = "balanced"
api_fallback = true

[context]
enabled = false
retrieval_enabled = false

[browser]
enabled = false
profile_root = "data/browser-profiles"
auth_vault_root = "data/browser-auth"

[chromium]
enabled = false
startup_timeout_seconds = 15
auto_recover = true
reconcile_interval_seconds = 15
extra_args = []

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
id = "coder-route"
account = "fake-account"
model = "fake-coder"
priority = 10
enabled = true
capabilities = ["chat", "coding", "reasoning"]
context_window = 65536

[virtual_models.llmgateway-auto]
routes = ["coder-route"]
EOF

python3 scripts/fake-openai.py >/tmp/llmgateway-native-agent-fake.log 2>&1 &
FAKE_PID=$!
cargo build --quiet
./target/debug/llmgateway >/tmp/llmgateway-native-agent.log 2>&1 &
PID=$!

cleanup() {
  kill "$PID" "$FAKE_PID" 2>/dev/null || true
  rm -f "$LLMGATEWAY_CONFIG"
  rm -f "$DB" "$DB-shm" "$DB-wal"
  rm -rf "$FAKE_BIN"
}
trap cleanup EXIT

for _ in {1..80}; do
  if curl -fsS "$LLMGATEWAY_BASE_URL/_llmgateway/health" >/dev/null; then
    break
  fi
  sleep 0.2
done

AGENT_HEALTH=$(./target/debug/llmgateway agent health)
printf '%s' "$AGENT_HEALTH" | grep -q '"status": "ok"'

AGENT_CAPS=$(./target/debug/llmgateway agent capabilities)
printf '%s' "$AGENT_CAPS" | grep -q 'llmgateway.agent.capabilities'
printf '%s' "$AGENT_CAPS" | grep -q 'coding'

AGENT_RESOLVE=$(./target/debug/llmgateway agent resolve   --model llmgateway-auto   --capability coding   --prompt "implement retry")
printf '%s' "$AGENT_RESOLVE" | grep -q '"route_id": "coder-route"'

AGENT_CHAT=$(./target/debug/llmgateway agent chat   llmgateway-auto "hello" --capability coding)
printf '%s' "$AGENT_CHAT" | grep -q 'fake reply'

BROWSER_SETTINGS=$(curl -fsS   "$LLMGATEWAY_BASE_URL/_llmgateway/browser-runtime/settings"   -H "Authorization: Bearer $LLMGATEWAY_API_KEY")
printf '%s' "$BROWSER_SETTINGS" | grep -q '"id":"google-chrome"'

BROWSER_SELECTED=$(curl -fsS -X PATCH   "$LLMGATEWAY_BASE_URL/_llmgateway/browser-runtime/settings"   -H "Authorization: Bearer $LLMGATEWAY_API_KEY"   -H "Content-Type: application/json"   -d '{"browser_id":"google-chrome"}')
printf '%s' "$BROWSER_SELECTED" | grep -q '"selected_browser_id":"google-chrome"'
grep -q "$FAKE_BIN/google-chrome" "$LLMGATEWAY_CONFIG"

DISCOVER=$(curl -fsS -X POST "$LLMGATEWAY_BASE_URL/mcp"   -H "Authorization: Bearer $LLMGATEWAY_API_KEY"   -H "Content-Type: application/json"   -H "MCP-Protocol-Version: 2026-07-28"   -H "Mcp-Method: server/discover"   -d '{"jsonrpc":"2.0","id":1,"method":"server/discover","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28"}}}')
printf '%s' "$DISCOVER" | grep -q '"resultType":"complete"'
printf '%s' "$DISCOVER" | grep -q '2026-07-28'

TOOLS=$(curl -fsS -X POST "$LLMGATEWAY_BASE_URL/mcp"   -H "Authorization: Bearer $LLMGATEWAY_API_KEY"   -H "Content-Type: application/json"   -H "MCP-Protocol-Version: 2026-07-28"   -H "Mcp-Method: tools/list"   -d '{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28"}}}')
printf '%s' "$TOOLS" | grep -q 'llmgateway_chat'
printf '%s' "$TOOLS" | grep -q 'llmgateway_resolve'

MCP_RESOLVE=$(curl -fsS -X POST "$LLMGATEWAY_BASE_URL/mcp"   -H "Authorization: Bearer $LLMGATEWAY_API_KEY"   -H "Content-Type: application/json"   -H "MCP-Protocol-Version: 2026-07-28"   -H "Mcp-Method: tools/call"   -H "Mcp-Name: llmgateway_resolve"   -d '{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"llmgateway_resolve","arguments":{"model":"llmgateway-auto","capabilities":["coding"]},"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28"}}}')
printf '%s' "$MCP_RESOLVE" | grep -q 'coder-route'
printf '%s' "$MCP_RESOLVE" | grep -q '"isError":false'

STDIO=$(printf '%s\n'   '{"jsonrpc":"2.0","id":4,"method":"server/discover","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28"}}}'   | env LLMGATEWAY_BASE_URL="$LLMGATEWAY_BASE_URL"       LLMGATEWAY_CLIENT_API_KEY="$LLMGATEWAY_API_KEY"       ./target/debug/llmgateway mcp --stdio)
printf '%s' "$STDIO" | grep -q '"resultType":"complete"'
printf '%s' "$STDIO" | grep -q '"name":"llmgateway"'

echo "native Agent CLI + MCP + browser runtime selection smoke passed"
