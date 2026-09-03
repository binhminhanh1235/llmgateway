#!/usr/bin/env bash
set -euo pipefail

export LLMGATEWAY_API_KEY="ci-local-key"
export FAKE_API_KEY="healthy"
export BROWSER_UNUSED="unused"
export LLMGATEWAY_CONFIG="/tmp/llmgateway-browser-provider-smoke.toml"
PROFILE_ROOT="/tmp/llmgateway-browser-provider-profiles"
BRIDGE_PID=""

rm -rf "$PROFILE_ROOT"
rm -f data/llmgateway.db data/llmgateway.db-shm data/llmgateway.db-wal
mkdir -p data

cat >/tmp/llmgateway-browser-provider-smoke.toml <<EOF
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
balance_weight = 0

[browser]
enabled = true
profile_root = "$PROFILE_ROOT"

[browser.sessions.fake-web]
provider = "browser-fake"
label = "Fake browser provider"
login_url = "http://127.0.0.1:18082/login"
enabled = true

[browser.bindings.browser-account]
session = "fake-web"

[context]
enabled = false
retrieval_enabled = false

[[providers]]
id = "browser-fake"
kind = "browser-http"
base_url = "http://127.0.0.1:18082/v1"
models_path = "models"

[[providers]]
id = "fake-api"
kind = "openai-compatible"
base_url = "http://127.0.0.1:18080/v1"
models_path = "models"

[[accounts]]
id = "browser-account"
provider = "browser-fake"
api_key_env = "BROWSER_UNUSED"
auth_style = "bearer"
enabled = true
discover_models = false

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
priority = 10
enabled = true
capabilities = ["chat"]

[[routes]]
id = "api-route"
account = "api-account"
model = "fake-model"
priority = 20
enabled = true
capabilities = ["chat"]

[virtual_models.llmgateway-auto]
routes = ["browser-route", "api-route"]
[virtual_models.llmgateway-coding]
routes = ["browser-route", "api-route"]
[virtual_models.llmgateway-best]
routes = ["browser-route", "api-route"]
EOF

python3 scripts/fake-openai.py >/tmp/llmgateway-browser-provider-api.log 2>&1 &
FAKE_PID=$!

python3 - <<'PY' >/tmp/llmgateway-browser-provider-bridge.log 2>&1 &
import http.server
import json
import socketserver

class Handler(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        if self.path != "/v1/chat/completions":
            self.send_response(404)
            self.end_headers()
            return
        length = int(self.headers.get("content-length", "0"))
        body = json.loads(self.rfile.read(length) or b"{}")
        assert self.headers.get("x-llmgateway-browser-session") == "fake-web"
        assert self.headers.get("x-llmgateway-browser-account") == "browser-account"
        assert self.headers.get("x-llmgateway-route") == "browser-route"
        assert body.get("model") == "browser-model"
        messages = body.get("messages") or []
        text = " ".join(str(message.get("content", "")) for message in messages)
        if "expire-browser" in text:
            payload = {"error": {"message": "browser session expired"}}
            raw = json.dumps(payload).encode()
            self.send_response(401)
        else:
            payload = {
                "id": "chatcmpl_browser",
                "object": "chat.completion",
                "model": "browser-model",
                "choices": [{"index": 0, "message": {"role": "assistant", "content": "browser-adapter-ok"}, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 4, "completion_tokens": 3, "total_tokens": 7},
            }
            raw = json.dumps(payload).encode()
            self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(raw)))
        self.end_headers()
        self.wfile.write(raw)

    def log_message(self, *_):
        pass

with socketserver.TCPServer(("127.0.0.1", 18082), Handler) as server:
    server.serve_forever()
PY
BRIDGE_PID=$!

cargo build --quiet
./target/debug/llmgateway >/tmp/llmgateway-browser-provider.log 2>&1 &
PID=$!
cleanup() {
  kill "$PID" "$FAKE_PID" "$BRIDGE_PID" 2>/dev/null || true
  rm -rf "$PROFILE_ROOT"
  rm -f data/llmgateway.db data/llmgateway.db-shm data/llmgateway.db-wal
  rm -f /tmp/llmgateway-browser-provider-smoke.toml
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

# A browser route whose session is not ready must be invisible to routing.
curl -fsS -D /tmp/browser-provider-before.headers -o /tmp/browser-provider-before.json \
  -X POST http://127.0.0.1:7331/v1/chat/completions \
  "${AUTH[@]}" "${JSON[@]}" \
  -d '{"model":"llmgateway-auto","stream":false,"messages":[{"role":"user","content":"before browser login"}]}'
grep -qi '^x-llmgateway-route: api-route' /tmp/browser-provider-before.headers

# Simulate the normal login lifecycle reaching READY.
curl -fsS -X POST http://127.0.0.1:7331/_llmgateway/browser-sessions/fake-web/login/start \
  "${AUTH[@]}" >/dev/null
curl -fsS -X POST http://127.0.0.1:7331/_llmgateway/browser-sessions/fake-web/login/complete \
  "${AUTH[@]}" >/dev/null

# Once READY, the higher-priority browser route should win and receive only opaque session identity.
curl -fsS -D /tmp/browser-provider-ready.headers -o /tmp/browser-provider-ready.json \
  -X POST http://127.0.0.1:7331/v1/chat/completions \
  "${AUTH[@]}" "${JSON[@]}" \
  -d '{"model":"llmgateway-auto","stream":false,"messages":[{"role":"user","content":"use browser route"}]}'
grep -qi '^x-llmgateway-route: browser-route' /tmp/browser-provider-ready.headers
python3 - /tmp/browser-provider-ready.json <<'PY'
import json,sys
with open(sys.argv[1], encoding="utf-8") as f:
    x=json.load(f)
assert x["choices"][0]["message"]["content"] == "browser-adapter-ok", x
PY

# 401 from a browser adapter means the session needs user attention, then the same request fails over.
curl -fsS -D /tmp/browser-provider-expired.headers -o /tmp/browser-provider-expired.json \
  -X POST http://127.0.0.1:7331/v1/chat/completions \
  "${AUTH[@]}" "${JSON[@]}" \
  -d '{"model":"llmgateway-auto","stream":false,"messages":[{"role":"user","content":"expire-browser"}]}'
grep -qi '^x-llmgateway-route: api-route' /tmp/browser-provider-expired.headers

SESSION=$(curl -fsS http://127.0.0.1:7331/_llmgateway/browser-sessions/fake-web "${AUTH[@]}")
printf '%s' "$SESSION" | python3 -c '
import json,sys
x=json.load(sys.stdin)
assert x["status"] == "requires_attention", x
assert "401" in (x.get("last_error") or ""), x
assert "browser session expired" in (x.get("last_error") or ""), x
'

# After the transition, future plans skip the browser route without calling the bridge again.
curl -fsS -D /tmp/browser-provider-after.headers -o /tmp/browser-provider-after.json \
  -X POST http://127.0.0.1:7331/v1/chat/completions \
  "${AUTH[@]}" "${JSON[@]}" \
  -d '{"model":"llmgateway-auto","stream":false,"messages":[{"role":"user","content":"after browser expiry"}]}'
grep -qi '^x-llmgateway-route: api-route' /tmp/browser-provider-after.headers

echo "llmgateway browser provider adapter + routing smoke test passed"
