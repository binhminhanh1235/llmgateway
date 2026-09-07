#!/usr/bin/env bash
set -euo pipefail

export LLMGATEWAY_API_KEY="ci-local-key"
export FAKE_API_KEY="healthy"
export LLMGATEWAY_CONFIG="/tmp/llmgateway-browser-reliability-smoke.toml"
PROFILE_ROOT="/tmp/llmgateway-browser-reliability-profiles"
FAKE_CHROMIUM="/tmp/llmgateway-reliability-fake-chromium"
LAUNCH_LOG="/tmp/llmgateway-reliability-launches.log"
GATEWAY_PID=""
BROWSER_PID=""
FAKE_PID=""

rm -rf "$PROFILE_ROOT"
rm -f "$LAUNCH_LOG"
rm -f data/llmgateway.db data/llmgateway.db-shm data/llmgateway.db-wal
mkdir -p data

cat >"$FAKE_CHROMIUM" <<'SH'
#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >>/tmp/llmgateway-reliability-launches.log
exec python3 scripts/fake-cdp-chromium.py "$@"
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
login_url = "https://chat.qwen.ai/"
enabled = true

[browser.sessions.fake-web-2]
provider = "browser-fake"
label = "Second cold browser"
login_url = "https://chat.qwen.ai/"
enabled = true

[browser.bindings.browser-account]
session = "fake-web"
transport_mode = "browser-only"
models = ["qwen-web-default"]

[browser.bindings.browser-account-2]
session = "fake-web-2"
transport_mode = "browser-only"
models = ["qwen-web-default"]

[browser_runtime]
allow_visible_auto_launch = false
max_running_browsers = 1
idle_timeout_secs = 2
session_ttl_secs = 20

[chromium]
enabled = true
executable = "$FAKE_CHROMIUM"
startup_timeout_seconds = 5
auto_recover = true
reconcile_interval_seconds = 5
extra_args = []

[chromium.sessions.fake-web]
enabled = true
ready_url_prefixes = ["https://chat.qwen.ai/"]

[chromium.sessions.fake-web-2]
enabled = true
ready_url_prefixes = ["https://chat.qwen.ai/"]

[context]
enabled = false
retrieval_enabled = false

[[providers]]
id = "browser-fake"
kind = "browser-qwen"
base_url = "https://chat.qwen.ai/"
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
id = "browser-account-2"
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
model = "qwen-web-default"
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
  kill "$FAKE_PID" 2>/dev/null || true
  rm -rf "$PROFILE_ROOT"
  rm -f "$FAKE_CHROMIUM" "$LLMGATEWAY_CONFIG" "$LAUNCH_LOG"
  rm -f data/llmgateway.db data/llmgateway.db-shm data/llmgateway.db-wal
}
trap cleanup EXIT

AUTH=(-H "Authorization: Bearer ${LLMGATEWAY_API_KEY}")
JSON=(-H "Content-Type: application/json")

start_gateway

# P4: gateway startup is browser-cold even with multiple configured browser accounts.
sleep 1
test ! -s "$LAUNCH_LOG"

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
printf '%s' "$LAUNCH" | python3 -c '
import json,sys
x=json.load(sys.stdin)
assert x["launch"]["visible"] is True, x
'
test "$(wc -l < "$LAUNCH_LOG" | tr -d ' ')" = "1"
grep -q -- '--new-window' "$LAUNCH_LOG"
if grep -q -- '--headless' "$LAUNCH_LOG"; then
  echo "explicit login unexpectedly launched headless" >&2
  exit 1
fi

VERIFY=$(curl -fsS -X POST \
  http://127.0.0.1:7331/_llmgateway/browser-sessions/fake-web/driver/verify \
  "${AUTH[@]}")
printf '%s' "$VERIFY" | python3 -c '
import json,sys
x=json.load(sys.stdin)
assert x["authenticated"] is True, x
assert x["browser_closed_after_capture"] is True, x
assert x["status"]["running"] is False, x
'
BROWSER_PID=""

INITIAL_READY_AT=$(curl -fsS http://127.0.0.1:7331/_llmgateway/browser-sessions/fake-web "${AUTH[@]}" \
  | python3 -c 'import json,sys; print(json.load(sys.stdin)["last_ready_at"])')

# P4: concurrent requests from one cold READY account share exactly one browser cold start.
REQUEST_PIDS=()
for i in $(seq 1 8); do
  curl -fsS -D "/tmp/browser-reliability-ready-$i.headers" -o "/tmp/browser-reliability-ready-$i.json" \
    -X POST http://127.0.0.1:7331/v1/chat/completions \
    "${AUTH[@]}" "${JSON[@]}" \
    -d '{"model":"llmgateway-auto","messages":[{"role":"user","content":"concurrent cold start"}]}' &
  REQUEST_PIDS+=("$!")
done
for request_pid in "${REQUEST_PIDS[@]}"; do
  wait "$request_pid"
done
for i in $(seq 1 8); do
  grep -qi '^x-llmgateway-route: browser-route' "/tmp/browser-reliability-ready-$i.headers"
done
test "$(wc -l < "$LAUNCH_LOG" | tr -d ' ')" = "2"
sed -n '2p' "$LAUNCH_LOG" | grep -q -- '--headless=new'
if sed -n '2p' "$LAUNCH_LOG" | grep -q -- '--new-window'; then
  echo "normal background request opened a visible Chromium window" >&2
  exit 1
fi
STATUS=$(curl -fsS http://127.0.0.1:7331/_llmgateway/browser-sessions/fake-web/driver/status "${AUTH[@]}")
BROWSER_PID=$(printf '%s' "$STATUS" | python3 -c 'import json,sys; print(json.load(sys.stdin)["pid"] or "")')
test -n "$BROWSER_PID"

# Restart llmgateway while the background browser is alive. Startup reconciliation reconnects
# to existing CDP and must not launch another process.
LAUNCHES_BEFORE_RESTART=$(wc -l < "$LAUNCH_LOG" | tr -d ' ')
stop_gateway
start_gateway
test "$(wc -l < "$LAUNCH_LOG" | tr -d ' ')" = "$LAUNCHES_BEFORE_RESTART"

SESSION=$(curl -fsS http://127.0.0.1:7331/_llmgateway/browser-sessions/fake-web "${AUTH[@]}")
printf '%s' "$SESSION" | python3 -c '
import json,sys
x=json.load(sys.stdin)
assert x["status"] == "ready", x
assert x["routable"] is True, x
'
RECONNECTED_READY_AT=$(printf '%s' "$SESSION" | python3 -c 'import json,sys; print(json.load(sys.stdin)["last_ready_at"])')
test "$RECONNECTED_READY_AT" = "$INITIAL_READY_AT"

STATUS=$(curl -fsS http://127.0.0.1:7331/_llmgateway/browser-sessions/fake-web/driver/status "${AUTH[@]}")
printf '%s' "$STATUS" | python3 -c '
import json,sys
x=json.load(sys.stdin)
assert x["running"] is True, x
assert x["managed"] is False, x
assert x["debugger_reachable"] is True, x
assert x["ready_match"] == "https://chat.qwen.ai/", x
'

# Simulate a Chromium crash. The stale DevToolsActivePort remains on disk.
kill "$BROWSER_PID"
wait "$BROWSER_PID" 2>/dev/null || true
BROWSER_PID=""

# P4: reconciliation may clean stale CDP, but it must never relaunch Chromium by itself.
LAUNCHES_AFTER_CRASH=$(wc -l < "$LAUNCH_LOG" | tr -d ' ')
sleep 6
test "$(wc -l < "$LAUNCH_LOG" | tr -d ' ')" = "$LAUNCHES_AFTER_CRASH"
STATUS=$(curl -fsS http://127.0.0.1:7331/_llmgateway/browser-sessions/fake-web/driver/status "${AUTH[@]}")
printf '%s' "$STATUS" | python3 -c '
import json,sys
x=json.load(sys.stdin)
assert x["running"] is False, x
'

# The next real request launches exactly one headless Chromium and recovers the route.
curl -fsS -D /tmp/browser-reliability-recovered.headers -o /tmp/browser-reliability-recovered.json \
  -X POST http://127.0.0.1:7331/v1/chat/completions \
  "${AUTH[@]}" "${JSON[@]}" \
  -d '{"model":"llmgateway-auto","messages":[{"role":"user","content":"after on-demand recovery"}]}'
grep -qi '^x-llmgateway-route: browser-route' /tmp/browser-reliability-recovered.headers
test "$(wc -l < "$LAUNCH_LOG" | tr -d ' ')" = "$((LAUNCHES_AFTER_CRASH + 1))"
tail -n 1 "$LAUNCH_LOG" | grep -q -- '--headless=new'
STATUS=$(curl -fsS http://127.0.0.1:7331/_llmgateway/browser-sessions/fake-web/driver/status "${AUTH[@]}")
BROWSER_PID=$(printf '%s' "$STATUS" | python3 -c 'import json,sys; print(json.load(sys.stdin)["pid"] or "")')
test -n "$BROWSER_PID"

# Idle reaping must stop the background process without changing the authenticated READY state.
LAUNCHES_BEFORE_IDLE=$(wc -l < "$LAUNCH_LOG" | tr -d ' ')
sleep 6
test "$(wc -l < "$LAUNCH_LOG" | tr -d ' ')" = "$LAUNCHES_BEFORE_IDLE"
STATUS=$(curl -fsS http://127.0.0.1:7331/_llmgateway/browser-sessions/fake-web/driver/status "${AUTH[@]}")
SESSION=$(curl -fsS http://127.0.0.1:7331/_llmgateway/browser-sessions/fake-web "${AUTH[@]}")
printf '%s\n%s' "$SESSION" "$STATUS" | python3 -c '
import json,sys
session,status=map(json.loads,sys.stdin.read().splitlines())
assert session["status"] == "ready", session
assert status["running"] is False, status
'
BROWSER_PID=""

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

echo "llmgateway P4 invisible browser lifecycle + on-demand recovery smoke test passed"
