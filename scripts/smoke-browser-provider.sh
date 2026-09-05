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

[browser.sessions.fake-web-secondary]
provider = "browser-fake"
label = "Fake browser provider secondary"
login_url = "http://127.0.0.1:18082/login"
enabled = true

[browser.bindings.browser-account]
session = "fake-web"

[browser.bindings.browser-account-secondary]
session = "fake-web-secondary"

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
id = "browser-account-secondary"
provider = "browser-fake"
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
id = "browser-route-secondary"
account = "browser-account-secondary"
model = "browser-model-secondary"
priority = 15
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
routes = ["browser-route", "browser-route-secondary", "api-route"]
[virtual_models.llmgateway-coding]
routes = ["browser-route", "browser-route-secondary", "api-route"]
[virtual_models.llmgateway-best]
routes = ["browser-route", "browser-route-secondary", "api-route"]
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
        account = self.headers.get("x-llmgateway-browser-account")
        route = self.headers.get("x-llmgateway-route")
        session = self.headers.get("x-llmgateway-browser-session")
        expected = {
            "browser-account": ("browser-route", "fake-web", "browser-model"),
            "browser-account-secondary": ("browser-route-secondary", "fake-web-secondary", "browser-model-secondary"),
        }
        assert account in expected, account
        expected_route, expected_session, expected_model = expected[account]
        assert route == expected_route, (route, expected_route)
        assert session == expected_session, (session, expected_session)
        assert body.get("model") == expected_model, body
        messages = body.get("messages") or []
        text = " ".join(str(message.get("content", "")) for message in messages)
        should_expire = (
            account == "browser-account" and "expire-browser" in text
        ) or (
            account == "browser-account-secondary" and "expire-secondary" in text
        )
        if should_expire:
            payload = {"error": {"message": f"{account} session expired"}}
            raw = json.dumps(payload).encode()
            self.send_response(401)
            content_type = "application/json"
        elif body.get("stream") is True:
            chunks = [
                {"id":"chatcmpl_browser_stream","object":"chat.completion.chunk","model":"browser-model","choices":[{"index":0,"delta":{"role":"assistant","content":"browser-stream-"},"finish_reason":None}]},
                {"id":"chatcmpl_browser_stream","object":"chat.completion.chunk","model":"browser-model","choices":[{"index":0,"delta":{"content":"ok"},"finish_reason":None}]},
                {"id":"chatcmpl_browser_stream","object":"chat.completion.chunk","model":"browser-model","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]},
            ]
            frames = [f"data: {json.dumps(chunk)}\n\n" for chunk in chunks]
            frames.append("data: [DONE]\n\n")
            raw = "".join(frames).encode()
            self.send_response(200)
            content_type = "text/event-stream"
        else:
            payload = {
                "id": "chatcmpl_browser",
                "object": "chat.completion",
                "model": expected_model,
                "choices": [{"index": 0, "message": {"role": "assistant", "content": f"{account}-ok"}, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 4, "completion_tokens": 3, "total_tokens": 7},
            }
            raw = json.dumps(payload).encode()
            self.send_response(200)
            content_type = "application/json"
        self.send_header("content-type", content_type)
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

TRANSPORT=$(curl -fsS http://127.0.0.1:7331/_llmgateway/accounts/browser-account/transport "${AUTH[@]}")
printf '%s' "$TRANSPORT" | python3 -c '
import json,sys
x=json.load(sys.stdin)
assert x["desired_policy"] == "browser-only", x
assert x["configured_mode"] == "auto", x
assert x["browserless"]["supported"] is False, x
assert x["browserless"]["recommended_mode"] is None, x
assert x["effective_transport"] == "unavailable", x
'

ALIAS=$(curl -fsS http://127.0.0.1:7331/accounts/browser-account/transport "${AUTH[@]}")
printf '%s' "$ALIAS" | python3 -c 'import json,sys; x=json.load(sys.stdin); assert x["account_id"] == "browser-account", x'

UNSUPPORTED_BODY=/tmp/llmgateway-browser-provider-transport-unsupported.json
UNSUPPORTED_STATUS=$(curl -sS -o "$UNSUPPORTED_BODY" -w '%{http_code}' -X PATCH \
  http://127.0.0.1:7331/_llmgateway/accounts/browser-account/transport \
  "${AUTH[@]}" "${JSON[@]}" -d '{"transport_policy":"browserless-preferred"}')
test "$UNSUPPORTED_STATUS" = "422"
python3 - "$UNSUPPORTED_BODY" <<'PY'
import json,sys
with open(sys.argv[1], encoding="utf-8") as f: x=json.load(f)
assert x["error"]["type"] == "browserless_unsupported", x
PY

INVALID_BODY=/tmp/llmgateway-browser-provider-transport-invalid.json
INVALID_STATUS=$(curl -sS -o "$INVALID_BODY" -w '%{http_code}' -X PATCH \
  http://127.0.0.1:7331/_llmgateway/accounts/browser-account/transport \
  "${AUTH[@]}" "${JSON[@]}" -d '{"transport_policy":"http-preferred"}')
test "$INVALID_STATUS" = "422"
python3 - "$INVALID_BODY" <<'PY'
import json,sys
with open(sys.argv[1], encoding="utf-8") as f: x=json.load(f)
assert x["error"]["type"] == "invalid_transport_policy", x
PY

NON_BROWSER_BODY=/tmp/llmgateway-browser-provider-transport-api.json
NON_BROWSER_STATUS=$(curl -sS -o "$NON_BROWSER_BODY" -w '%{http_code}' \
  http://127.0.0.1:7331/_llmgateway/accounts/api-account/transport "${AUTH[@]}")
test "$NON_BROWSER_STATUS" = "409"
python3 - "$NON_BROWSER_BODY" <<'PY'
import json,sys
with open(sys.argv[1], encoding="utf-8") as f: x=json.load(f)
assert x["error"]["type"] == "account_transport_not_browser_backed", x
PY

MISSING_TRANSPORT_BODY=/tmp/llmgateway-browser-provider-transport-missing.json
MISSING_TRANSPORT_STATUS=$(curl -sS -o "$MISSING_TRANSPORT_BODY" -w '%{http_code}' \
  http://127.0.0.1:7331/_llmgateway/accounts/missing-account/transport "${AUTH[@]}")
test "$MISSING_TRANSPORT_STATUS" = "404"
python3 - "$MISSING_TRANSPORT_BODY" <<'PY'
import json,sys
with open(sys.argv[1], encoding="utf-8") as f: x=json.load(f)
assert x["error"]["type"] == "account_transport_not_found", x
PY

BROWSER_ONLY=$(curl -fsS -X PATCH \
  http://127.0.0.1:7331/_llmgateway/accounts/browser-account/transport \
  "${AUTH[@]}" "${JSON[@]}" -d '{"transport_policy":"browser-only"}')
printf '%s' "$BROWSER_ONLY" | python3 -c '
import json,sys
x=json.load(sys.stdin)
assert x["desired_policy"] == "browser-only", x
assert x["configured_mode"] == "browser-only", x
assert x["browserless"]["supported"] is False, x
'

python3 - "$LLMGATEWAY_CONFIG" <<'PY'
import sys,tomllib
with open(sys.argv[1], "rb") as f: x=tomllib.load(f)
assert x["browser"]["bindings"]["browser-account"]["transport_mode"] == "browser-only", x["browser"]["bindings"]["browser-account"]
PY
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
curl -fsS -X POST http://127.0.0.1:7331/_llmgateway/browser-sessions/fake-web-secondary/login/start \
  "${AUTH[@]}" >/dev/null
curl -fsS -X POST http://127.0.0.1:7331/_llmgateway/browser-sessions/fake-web-secondary/login/complete \
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
assert x["choices"][0]["message"]["content"] == "browser-account-ok", x
PY

# Streaming uses the same adapter lane and remains OpenAI-compatible end to end.
curl -fsS -N -D /tmp/browser-provider-stream.headers -o /tmp/browser-provider-stream.txt \
  -X POST http://127.0.0.1:7331/v1/chat/completions \
  "${AUTH[@]}" "${JSON[@]}" \
  -d '{"model":"llmgateway-auto","stream":true,"messages":[{"role":"user","content":"stream through browser route"}]}'
grep -qi '^x-llmgateway-route: browser-route' /tmp/browser-provider-stream.headers
python3 - /tmp/browser-provider-stream.txt <<'PY'
import json,sys
text=""
with open(sys.argv[1], encoding="utf-8") as f:
    for line in f:
        if not line.startswith("data: "):
            continue
        data=line[len("data: "):].strip()
        if data == "[DONE]":
            continue
        chunk=json.loads(data)
        choice=(chunk.get("choices") or [{}])[0]
        text += (choice.get("delta") or {}).get("content") or ""
assert text == "browser-stream-ok", text
PY

# 401 from the primary browser marks only that session for attention.
# The same request must fail over to the second browser account before considering API.
curl -fsS -D /tmp/browser-provider-expired.headers -o /tmp/browser-provider-expired.json \
  -X POST http://127.0.0.1:7331/v1/chat/completions \
  "${AUTH[@]}" "${JSON[@]}" \
  -d '{"model":"llmgateway-auto","stream":false,"messages":[{"role":"user","content":"expire-browser"}]}'
grep -qi '^x-llmgateway-route: browser-route-secondary' /tmp/browser-provider-expired.headers
python3 - /tmp/browser-provider-expired.json <<'PY'
import json,sys
with open(sys.argv[1], encoding="utf-8") as f:
    x=json.load(f)
assert x["choices"][0]["message"]["content"] == "browser-account-secondary-ok", x
PY

SESSION=$(curl -fsS http://127.0.0.1:7331/_llmgateway/browser-sessions/fake-web "${AUTH[@]}")
printf '%s' "$SESSION" | python3 -c '
import json,sys
x=json.load(sys.stdin)
assert x["status"] == "requires_attention", x
assert "401" in (x.get("last_error") or ""), x
assert "browser-account session expired" in (x.get("last_error") or ""), x
'

# Future plans skip the expired primary and keep using the healthy secondary browser.
curl -fsS -D /tmp/browser-provider-after.headers -o /tmp/browser-provider-after.json \
  -X POST http://127.0.0.1:7331/v1/chat/completions \
  "${AUTH[@]}" "${JSON[@]}" \
  -d '{"model":"llmgateway-auto","stream":false,"messages":[{"role":"user","content":"after primary browser expiry"}]}'
grep -qi '^x-llmgateway-route: browser-route-secondary' /tmp/browser-provider-after.headers

# Expire the second browser too. Only after every browser account is unavailable may API win.
curl -fsS -D /tmp/browser-provider-all-expired.headers -o /tmp/browser-provider-all-expired.json \
  -X POST http://127.0.0.1:7331/v1/chat/completions \
  "${AUTH[@]}" "${JSON[@]}" \
  -d '{"model":"llmgateway-auto","stream":false,"messages":[{"role":"user","content":"expire-secondary"}]}'
grep -qi '^x-llmgateway-route: api-route' /tmp/browser-provider-all-expired.headers

SECONDARY=$(curl -fsS http://127.0.0.1:7331/_llmgateway/browser-sessions/fake-web-secondary "${AUTH[@]}")
printf '%s' "$SECONDARY" | python3 -c '
import json,sys
x=json.load(sys.stdin)
assert x["status"] == "requires_attention", x
assert "browser-account-secondary session expired" in (x.get("last_error") or ""), x
'

echo "llmgateway browser-to-browser failover + API fallback + streaming smoke test passed"
