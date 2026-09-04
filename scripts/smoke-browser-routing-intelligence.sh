#!/usr/bin/env bash
set -euo pipefail

export LLMGATEWAY_API_KEY="ci-browser-routing-key"
export FAKE_API_KEY="healthy"
export LLMGATEWAY_CONFIG="/tmp/llmgateway-browser-routing-intelligence.toml"
PROFILE_ROOT="/tmp/llmgateway-browser-routing-intelligence-profiles"
FAKE_CHROMIUM="/tmp/llmgateway-browser-routing-fake-chromium"
GATEWAY_PID=""
BROWSER_A_PID=""
BROWSER_B_PID=""
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
test -n "$profile"
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
                {"type": "page", "url": "http://127.0.0.1:18086/ready"}
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
execution_preference = "prefer-browser"
api_fallback = true
adaptive_enabled = false
task_aware_enabled = false
browser_fairness_enabled = true
browser_recovery_penalty = 8
browser_recovery_max_penalty = 40
browser_sticky_affinity = true

[browser]
enabled = true
profile_root = "$PROFILE_ROOT"

[browser.sessions.fake-a]
provider = "browser-fake"
label = "Fake browser A"
login_url = "http://127.0.0.1:18086/login"
enabled = true

[browser.sessions.fake-b]
provider = "browser-fake"
label = "Fake browser B"
login_url = "http://127.0.0.1:18086/login"
enabled = true

[browser.bindings.browser-a]
session = "fake-a"

[browser.bindings.browser-b]
session = "fake-b"

[chromium]
enabled = true
executable = "$FAKE_CHROMIUM"
startup_timeout_seconds = 5
auto_recover = false
reconcile_interval_seconds = 5
extra_args = []

[chromium.sessions.fake-a]
enabled = true
ready_url_prefixes = ["http://127.0.0.1:18086/ready"]

[chromium.sessions.fake-b]
enabled = true
ready_url_prefixes = ["http://127.0.0.1:18086/ready"]

[context]
enabled = false
retrieval_enabled = false

[[providers]]
id = "browser-fake"
kind = "browser-http"
base_url = "http://127.0.0.1:18085/v1"
models_path = "models"

[[providers]]
id = "fake-api"
kind = "openai-compatible"
base_url = "http://127.0.0.1:18080/v1"
models_path = "models"

[[accounts]]
id = "browser-a"
provider = "browser-fake"
enabled = true
discover_models = false

[[accounts]]
id = "browser-b"
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
id = "browser-a-route"
account = "browser-a"
model = "browser-model"
priority = 10
enabled = true
capabilities = ["chat"]

[[routes]]
id = "browser-b-route"
account = "browser-b"
model = "browser-model"
priority = 10
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
routes = ["api-route", "browser-a-route", "browser-b-route"]
EOF

python3 scripts/fake-openai.py >/tmp/llmgateway-browser-routing-api.log 2>&1 &
FAKE_PID=$!

python3 - <<'PY' >/tmp/llmgateway-browser-routing-bridge.log 2>&1 &
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
        session = self.headers.get("x-llmgateway-browser-session", "")
        text = json.dumps(body)
        if session == "fake-a" and "force-recovery" in text:
            raw = json.dumps({"error": {"message": "forced browser A retry"}}).encode()
            self.send_response(409)
            self.send_header("content-type", "application/json")
            self.send_header("content-length", str(len(raw)))
            self.end_headers()
            self.wfile.write(raw)
            return
        payload = {
            "id": "chatcmpl_browser_routing",
            "object": "chat.completion",
            "model": body.get("model"),
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": f"{session}-ok"},
                "finish_reason": "stop"
            }],
        }
        raw = json.dumps(payload).encode()
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(raw)))
        self.end_headers()
        self.wfile.write(raw)

    def log_message(self, *_):
        pass

with socketserver.TCPServer(("127.0.0.1", 18085), Handler) as server:
    server.serve_forever()
PY
BRIDGE_PID=$!

cargo build --quiet
./target/debug/llmgateway >/tmp/llmgateway-browser-routing.log 2>&1 &
GATEWAY_PID=$!

cleanup() {
  kill "$GATEWAY_PID" "$FAKE_PID" "$BRIDGE_PID" 2>/dev/null || true
  if [ -n "$BROWSER_A_PID" ]; then kill "$BROWSER_A_PID" 2>/dev/null || true; fi
  if [ -n "$BROWSER_B_PID" ]; then kill "$BROWSER_B_PID" 2>/dev/null || true; fi
  rm -rf "$PROFILE_ROOT"
  rm -f "$FAKE_CHROMIUM" "$LLMGATEWAY_CONFIG"
  rm -f data/llmgateway.db data/llmgateway.db-shm data/llmgateway.db-wal
}
trap cleanup EXIT

for _ in {1..80}; do
  if curl -fsS http://127.0.0.1:7331/_llmgateway/health >/dev/null; then
    break
  fi
  sleep 0.2
done

AUTH=(-H "Authorization: Bearer ${LLMGATEWAY_API_KEY}")
JSON=(-H "Content-Type: application/json")

launch_verify() {
  local session="$1"
  local launch
  launch=$(curl -fsS -X POST "http://127.0.0.1:7331/_llmgateway/browser-sessions/${session}/driver/launch" "${AUTH[@]}")
  curl -fsS -X POST "http://127.0.0.1:7331/_llmgateway/browser-sessions/${session}/driver/verify" "${AUTH[@]}" >/dev/null
  printf '%s' "$launch" | python3 -c 'import json,sys; print(json.load(sys.stdin)["launch"]["pid"])'
}

BROWSER_A_PID=$(launch_verify fake-a)
BROWSER_B_PID=$(launch_verify fake-b)

explain() {
  curl -fsS -X POST http://127.0.0.1:7331/_llmgateway/routes/explain \
    "${AUTH[@]}" "${JSON[@]}" \
    -d '{"model":"llmgateway-auto","body":{"messages":[{"role":"user","content":"routing explain"}]}}'
}

route_request() {
  local prompt="$1"
  curl -fsS -D /tmp/browser-routing.headers -o /tmp/browser-routing.json \
    -X POST http://127.0.0.1:7331/v1/chat/completions \
    "${AUTH[@]}" "${JSON[@]}" \
    -d "{\"model\":\"llmgateway-auto\",\"stream\":false,\"messages\":[{\"role\":\"user\",\"content\":\"${prompt}\"}]}"
  awk 'BEGIN{IGNORECASE=1} /^x-llmgateway-route:/ {gsub("\r", "", $2); print $2}' /tmp/browser-routing.headers
}

INITIAL=$(explain)
printf '%s' "$INITIAL" | python3 -c '
import json,sys
x=json.load(sys.stdin)
assert x["execution_policy"] == "prefer-browser", x
assert x["selected_route"] == "browser-a-route", x
c={v["route_id"]:v for v in x["candidates"]}
assert c["browser-a-route"]["browser_fairness_rank"] == 0, c
assert c["browser-b-route"]["browser_fairness_rank"] == 0, c
assert c["browser-a-route"]["policy_reason"] == "browser_preferred", c
assert c["api-route"]["policy_reason"] == "api_fallback", c
'

FIRST=$(route_request first)
test "$FIRST" = "browser-a-route"

AFTER_FIRST=$(explain)
printf '%s' "$AFTER_FIRST" | python3 -c '
import json,sys
x=json.load(sys.stdin)
c={v["route_id"]:v for v in x["candidates"]}
assert x["selected_route"] == "browser-b-route", x
assert c["browser-a-route"]["browser_fairness_rank"] > c["browser-b-route"]["browser_fairness_rank"], c
'

SECOND=$(route_request second)
test "$SECOND" = "browser-b-route"

AFTER_SECOND=$(explain)
printf '%s' "$AFTER_SECOND" | python3 -c '
import json,sys
x=json.load(sys.stdin)
assert x["selected_route"] == "browser-a-route", x
'

THREAD=$(curl -fsS -X POST http://127.0.0.1:7331/v1/threads \
  "${AUTH[@]}" "${JSON[@]}" -d '{"model":"llmgateway-auto"}')
THREAD_ID=$(printf '%s' "$THREAD" | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')

curl -fsS -D /tmp/browser-routing-thread-1.headers -o /tmp/browser-routing-thread-1.json \
  -X POST "http://127.0.0.1:7331/v1/threads/${THREAD_ID}/messages" \
  "${AUTH[@]}" "${JSON[@]}" -d '{"content":"thread one","stream":false}'
grep -qi '^x-llmgateway-route: browser-a-route' /tmp/browser-routing-thread-1.headers

# Fairness now prefers B, but the healthy browser session already attached to this thread stays sticky.
curl -fsS -D /tmp/browser-routing-thread-2.headers -o /tmp/browser-routing-thread-2.json \
  -X POST "http://127.0.0.1:7331/v1/threads/${THREAD_ID}/messages" \
  "${AUTH[@]}" "${JSON[@]}" -d '{"content":"thread two","stream":false}'
grep -qi '^x-llmgateway-route: browser-a-route' /tmp/browser-routing-thread-2.headers

# Rotate B once so A becomes the least-recently-used route again.
ROTATE=$(route_request rotate-before-recovery)
test "$ROTATE" = "browser-b-route"

# A returns a retryable conflict. Gateway falls through to B and records browser recovery state.
RECOVER=$(route_request force-recovery)
test "$RECOVER" = "browser-b-route"

COOLING=$(explain)
printf '%s' "$COOLING" | python3 -c '
import json,sys
x=json.load(sys.stdin)
c={v["route_id"]:v for v in x["candidates"]}
assert c["browser-a-route"]["eligible"] is False, c["browser-a-route"]
assert "route_cooldown" in c["browser-a-route"]["exclusion_reasons"], c["browser-a-route"]
'

sleep 11

RECOVERY=$(explain)
printf '%s' "$RECOVERY" | python3 -c '
import json,sys
x=json.load(sys.stdin)
c={v["route_id"]:v for v in x["candidates"]}
a=c["browser-a-route"]
b=c["browser-b-route"]
assert a["eligible"] is True, a
assert a["browser_recovery_penalty"] == 8, a
assert "browser_recovery_probe" in a["warnings"], a
assert a["final_score"] > b["final_score"], (a,b)
assert x["selected_route"] == "browser-b-route", x
'

# With both browser sessions deliberately stopped, paid/API execution becomes the deterministic fallback.
curl -fsS -X POST http://127.0.0.1:7331/_llmgateway/browser-sessions/fake-a/driver/stop "${AUTH[@]}" >/dev/null
curl -fsS -X POST http://127.0.0.1:7331/_llmgateway/browser-sessions/fake-b/driver/stop "${AUTH[@]}" >/dev/null
BROWSER_A_PID=""
BROWSER_B_PID=""

FALLBACK=$(route_request api-fallback)
test "$FALLBACK" = "api-route"

echo "llmgateway browser-aware routing intelligence smoke test passed"
