#!/usr/bin/env bash
set -euo pipefail

export LLMGATEWAY_API_KEY="ci-local-key"
export FAKE_API_KEY="healthy"
export LLMGATEWAY_CONFIG="/tmp/llmgateway-browser-reliability-smoke.toml"
PROFILE_ROOT="/tmp/llmgateway-browser-reliability-profiles"
FAKE_CHROMIUM="/tmp/llmgateway-reliability-fake-chromium"
GATEWAY_PID=""
BROWSER_PID=""
BRIDGE_PID=""
FAKE_PID=""

rm -rf "$PROFILE_ROOT"
rm -f data/llmgateway.db data/llmgateway.db-shm data/llmgateway.db-wal
mkdir -p data

cat >"$FAKE_CHROMIUM" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
profile=""
for arg in "$@"; do
  case "$arg" in
    --user-data-dir=*) profile="${arg#--user-data-dir=}" ;;
  esac
done
if [ -z "$profile" ]; then
  exit 2
fi
mkdir -p "$profile"
exec python3 - "$profile" <<'PY'
import http.server
import json
import os
import socketserver
import sys

profile = sys.argv[1]

class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        if self.path == "/json/list":
            body = json.dumps([
                {"type": "page", "url": "http://127.0.0.1:18084/ready?token=hidden#fragment"}
            ]).encode()
            self.send_response(200)
            self.send_header("content-type", "application/json")
            self.send_header("content-length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        self.send_response(404)
        self.end_headers()

    def log_message(self, *_):
        pass

with socketserver.TCPServer(("127.0.0.1", 0), Handler) as server:
    port = server.server_address[1]
    with open(os.path.join(profile, "DevToolsActivePort"), "w", encoding="utf-8") as f:
        f.write(f"{port}\n/devtools/browser/fake\n")
    server.serve_forever()
PY
SH
chmod 700 "$FAKE_CHROMIUM"

cat >"$LLMGATEWAY_CONFIG" <<EOF
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

[routing]
execution_preference = "browser-first"
api_fallback = true
adaptive_enabled = false
task_aware_enabled = false

[browser]
enabled = true
profile_root = "$PROFILE_ROOT"

[browser.sessions.fake-web]
provider = "browser-fake"
label = "Reliable fake browser"
login_url = "http://127.0.0.1:18084/login"
enabled = true

[browser.bindings.browser-account]
session = "fake-web"

[chromium]
enabled = true
executable = "$FAKE_CHROMIUM"
startup_timeout_seconds = 5
auto_recover = true
reconcile_interval_seconds = 5
extra_args = []

[chromium.sessions.fake-web]
enabled = true
ready_url_prefixes = ["http://127.0.0.1:18084/ready"]

[context]
enabled = false
retrieval_enabled = false

[[providers]]
id = "browser-fake"
kind = "browser-http"
base_url = "http://127.0.0.1:18083/v1"
models_path = "models"

[[providers]]
id = "fake-api"
kind = "openai-compatible"
base_url = "http://127.0.0.1:18080/v1"
models_path = "models"

[[accounts]]
id = "browser-account"
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
priority = 100
enabled = true
capabilities = ["chat"]

[[routes]]
id = "api-route"
account = "api-account"
model = "fake-model"
priority = 1
enabled = true
capabilities = ["chat"]

[virtual_models.llmgateway-auto]
routes = ["api-route", "browser-route"]
EOF

python3 scripts/fake-openai.py >/tmp/llmgateway-browser-reliability-api.log 2>&1 &
FAKE_PID=$!

python3 - <<'PY' >/tmp/llmgateway-browser-reliability-bridge.log 2>&1 &
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
        payload = {
            "id": "chatcmpl_browser_reliability",
            "object": "chat.completion",
            "model": body.get("model"),
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "browser-reliability-ok"}, "finish_reason": "stop"}],
        }
        raw = json.dumps(payload).encode()
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(raw)))
        self.end_headers()
        self.wfile.write(raw)

    def log_message(self, *_):
        pass

with socketserver.TCPServer(("127.0.0.1", 18083), Handler) as server:
    server.serve_forever()
PY
BRIDGE_PID=$!

cargo build --quiet

start_gateway() {
  ./target/debug/llmgateway >/tmp/llmgateway-browser-reliability.log 2>&1 &
  GATEWAY_PID=$!
  for _ in {1..80}; do
    if curl -fsS http://127.0.0.1:7331/_llmgateway/health >/dev/null; then
      return
    fi
    sleep 0.2
  done
  echo "gateway failed to start" >&2
  cat /tmp/llmgateway-browser-reliability.log >&2 || true
  exit 1
}

stop_gateway() {
  if [ -n "$GATEWAY_PID" ]; then
    kill "$GATEWAY_PID" 2>/dev/null || true
    wait "$GATEWAY_PID" 2>/dev/null || true
    GATEWAY_PID=""
  fi
}

cleanup() {
  stop_gateway
  if [ -n "$BROWSER_PID" ]; then
    kill "$BROWSER_PID" 2>/dev/null || true
  fi
  kill "$FAKE_PID" "$BRIDGE_PID" 2>/dev/null || true
  rm -rf "$PROFILE_ROOT"
  rm -f "$FAKE_CHROMIUM" "$LLMGATEWAY_CONFIG"
  rm -f data/llmgateway.db data/llmgateway.db-shm data/llmgateway.db-wal
}
trap cleanup EXIT

AUTH=(-H "Authorization: Bearer ${LLMGATEWAY_API_KEY}")
JSON=(-H "Content-Type: application/json")

start_gateway

# Browser is not logged in yet, so the lower-priority API route is the only eligible route.
curl -fsS -D /tmp/browser-reliability-before.headers -o /tmp/browser-reliability-before.json \
  -X POST http://127.0.0.1:7331/v1/chat/completions \
  "${AUTH[@]}" "${JSON[@]}" \
  -d '{"model":"llmgateway-auto","messages":[{"role":"user","content":"before login"}]}'
grep -qi '^x-llmgateway-route: api-route' /tmp/browser-reliability-before.headers

LAUNCH=$(curl -fsS -X POST \
  http://127.0.0.1:7331/_llmgateway/browser-sessions/fake-web/driver/launch \
  "${AUTH[@]}")
BROWSER_PID=$(printf '%s' "$LAUNCH" | python3 -c 'import json,sys; print(json.load(sys.stdin)["launch"]["pid"])')
curl -fsS -X POST \
  http://127.0.0.1:7331/_llmgateway/browser-sessions/fake-web/driver/verify \
  "${AUTH[@]}" >/dev/null

# Browser-first must beat the API route even though API has priority 1 and browser has 100.
curl -fsS -D /tmp/browser-reliability-ready.headers -o /tmp/browser-reliability-ready.json \
  -X POST http://127.0.0.1:7331/v1/chat/completions \
  "${AUTH[@]}" "${JSON[@]}" \
  -d '{"model":"llmgateway-auto","messages":[{"role":"user","content":"browser first"}]}'
grep -qi '^x-llmgateway-route: browser-route' /tmp/browser-reliability-ready.headers

# Restart llmgateway but leave Chromium alive. Startup reconciliation must reconnect to the live profile.
stop_gateway
start_gateway

SESSION=$(curl -fsS http://127.0.0.1:7331/_llmgateway/browser-sessions/fake-web "${AUTH[@]}")
printf '%s' "$SESSION" | python3 -c '
import json,sys
x=json.load(sys.stdin)
assert x["status"] == "ready", x
assert x["routable"] is True, x
'

STATUS=$(curl -fsS http://127.0.0.1:7331/_llmgateway/browser-sessions/fake-web/driver/status "${AUTH[@]}")
printf '%s' "$STATUS" | python3 -c '
import json,sys
x=json.load(sys.stdin)
assert x["running"] is True, x
assert x["managed"] is False, x
assert x["debugger_reachable"] is True, x
assert x["ready_match"] == "http://127.0.0.1:18084/ready", x
'

# Simulate a Chromium crash. The stale DevToolsActivePort remains on disk.
kill "$BROWSER_PID"
wait "$BROWSER_PID" 2>/dev/null || true
BROWSER_PID=""

# Background reconciliation should remove the stale port, relaunch the isolated profile,
# verify the authenticated page, and make the browser route eligible again.
RECOVERED=0
for _ in {1..40}; do
  sleep 0.5
  SESSION=$(curl -fsS http://127.0.0.1:7331/_llmgateway/browser-sessions/fake-web "${AUTH[@]}")
  STATUS=$(curl -fsS http://127.0.0.1:7331/_llmgateway/browser-sessions/fake-web/driver/status "${AUTH[@]}")
  if printf '%s\n%s' "$SESSION" "$STATUS" | python3 -c '
import json,sys
lines=sys.stdin.read().splitlines()
session=json.loads(lines[0])
status=json.loads(lines[1])
ok=session["status"] == "ready" and session["routable"] is True and status["running"] is True and status["managed"] is True and status["ready_match"] == "http://127.0.0.1:18084/ready"
raise SystemExit(0 if ok else 1)
'; then
    BROWSER_PID=$(printf '%s' "$STATUS" | python3 -c 'import json,sys; print(json.load(sys.stdin)["pid"] or "")')
    RECOVERED=1
    break
  fi
done
if [ "$RECOVERED" != "1" ]; then
  echo "browser did not recover" >&2
  cat /tmp/llmgateway-browser-reliability.log >&2 || true
  exit 1
fi

curl -fsS -D /tmp/browser-reliability-recovered.headers -o /tmp/browser-reliability-recovered.json \
  -X POST http://127.0.0.1:7331/v1/chat/completions \
  "${AUTH[@]}" "${JSON[@]}" \
  -d '{"model":"llmgateway-auto","messages":[{"role":"user","content":"after automatic recovery"}]}'
grep -qi '^x-llmgateway-route: browser-route' /tmp/browser-reliability-recovered.headers

# A deliberate stop is sticky state: the background reconciler must not relaunch it.
curl -fsS -X POST \
  http://127.0.0.1:7331/_llmgateway/browser-sessions/fake-web/driver/stop \
  "${AUTH[@]}" >/dev/null
BROWSER_PID=""
sleep 6

SESSION=$(curl -fsS http://127.0.0.1:7331/_llmgateway/browser-sessions/fake-web "${AUTH[@]}")
STATUS=$(curl -fsS http://127.0.0.1:7331/_llmgateway/browser-sessions/fake-web/driver/status "${AUTH[@]}")
printf '%s\n%s' "$SESSION" "$STATUS" | python3 -c '
import json,sys
lines=sys.stdin.read().splitlines()
session=json.loads(lines[0])
status=json.loads(lines[1])
assert session["status"] == "stopped", session
assert session["routable"] is False, session
assert status["running"] is False, status
'

curl -fsS -D /tmp/browser-reliability-stopped.headers -o /tmp/browser-reliability-stopped.json \
  -X POST http://127.0.0.1:7331/v1/chat/completions \
  "${AUTH[@]}" "${JSON[@]}" \
  -d '{"model":"llmgateway-auto","messages":[{"role":"user","content":"after deliberate stop"}]}'
grep -qi '^x-llmgateway-route: api-route' /tmp/browser-reliability-stopped.headers

echo "llmgateway browser restart + stale-CDP + automatic recovery smoke test passed"
