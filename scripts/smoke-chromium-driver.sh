#!/usr/bin/env bash
set -euo pipefail

export LLMGATEWAY_API_KEY="ci-local-key"
export FAKE_API_KEY="fake-key"
export LLMGATEWAY_CONFIG="/tmp/llmgateway-chromium-driver-smoke.toml"
PROFILE_ROOT="/tmp/llmgateway-chromium-profiles"
FAKE_CHROMIUM="/tmp/llmgateway-fake-chromium"
BROWSER_PID=""

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
  echo "missing --user-data-dir" >&2
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
                {
                    "type": "page",
                    "url": "http://127.0.0.1:18081/ready?token=super-secret#fragment"
                }
            ]).encode()
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
        else:
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

cat >/tmp/llmgateway-chromium-driver-smoke.toml <<EOF
[server]
host = "127.0.0.1"
port = 7331

[api]
key_env = "LLMGATEWAY_API_KEY"
default_model = "llmgateway-auto"

[storage]
database_url = "sqlite://data/llmgateway.db"

[browser]
enabled = true
profile_root = "$PROFILE_ROOT"

[browser.sessions.fake-web]
provider = "fake"
label = "Fake browser account"
login_url = "http://127.0.0.1:18080/login"
enabled = true

[chromium]
enabled = true
executable = "$FAKE_CHROMIUM"
startup_timeout_seconds = 5
extra_args = []

[chromium.sessions.fake-web]
enabled = true
ready_url_prefixes = ["http://127.0.0.1:18081/ready"]

[context]
enabled = false
retrieval_enabled = false

[[providers]]
id = "fake"
kind = "openai-compatible"
base_url = "http://127.0.0.1:18080/v1"
models_path = "models"

[[accounts]]
id = "fake-api"
provider = "fake"
api_key_env = "FAKE_API_KEY"
auth_style = "bearer"
enabled = true
discover_models = false

[[routes]]
id = "fake-route"
account = "fake-api"
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

cargo build --quiet
./target/debug/llmgateway >/tmp/llmgateway-chromium-driver.log 2>&1 &
PID=$!
cleanup() {
  kill "$PID" 2>/dev/null || true
  if [ -n "$BROWSER_PID" ]; then
    kill "$BROWSER_PID" 2>/dev/null || true
  fi
  rm -rf "$PROFILE_ROOT"
  rm -f "$FAKE_CHROMIUM" /tmp/llmgateway-chromium-driver-smoke.toml
  rm -f data/llmgateway.db data/llmgateway.db-shm data/llmgateway.db-wal
}
trap cleanup EXIT

for _ in {1..50}; do
  if curl -fsS http://127.0.0.1:7331/_llmgateway/health >/dev/null; then
    break
  fi
  sleep 0.2
done

AUTH=(-H "Authorization: Bearer ${LLMGATEWAY_API_KEY}")

# Browser Accounts UI must be bundled into the Rust binary and wired to the real driver endpoints.
UI_HTML=$(curl -fsS http://127.0.0.1:7331/)
BROWSER_JS=$(curl -fsS http://127.0.0.1:7331/ui/browser-control.js)
BROWSER_CSS=$(curl -fsS http://127.0.0.1:7331/ui/browser-control.css)
grep -q '/ui/browser-control.js' <<<"$UI_HTML"
grep -q '/ui/browser-control.css' <<<"$UI_HTML"
grep -q '/driver/launch' <<<"$BROWSER_JS"
grep -q '/driver/verify' <<<"$BROWSER_JS"
grep -q 'login_required' <<<"$BROWSER_JS"
grep -q '.browser-control-panel' <<<"$BROWSER_CSS"

LAUNCH=$(curl -fsS -X POST \
  http://127.0.0.1:7331/_llmgateway/browser-sessions/fake-web/driver/launch \
  "${AUTH[@]}")
BROWSER_PID=$(printf '%s' "$LAUNCH" | python3 -c 'import json,sys; print(json.load(sys.stdin)["launch"]["pid"] or "")')
printf '%s' "$LAUNCH" | python3 -c '
import json,sys
x=json.load(sys.stdin)
assert x["launched"] is True, x
launch=x["launch"]
assert launch["session_id"] == "fake-web", launch
assert launch["login_attempt_id"].startswith("browser_login_"), launch
assert launch["debugger_port"] > 0, launch
assert launch["profile_dir"].endswith("/fake-web"), launch
'

STATUS=$(curl -fsS \
  http://127.0.0.1:7331/_llmgateway/browser-sessions/fake-web/driver/status \
  "${AUTH[@]}")
printf '%s' "$STATUS" | python3 -c '
import json,sys
x=json.load(sys.stdin)
assert x["running"] is True, x
assert x["debugger_port"] > 0, x
assert x["ready_match"] == "http://127.0.0.1:18081/ready", x
assert x["pages"][0]["location"] == "http://127.0.0.1:18081/ready", x
serialized=json.dumps(x)
assert "super-secret" not in serialized, x
assert "token=" not in serialized, x
'

VERIFY=$(curl -fsS -X POST \
  http://127.0.0.1:7331/_llmgateway/browser-sessions/fake-web/driver/verify \
  "${AUTH[@]}")
printf '%s' "$VERIFY" | python3 -c '
import json,sys
x=json.load(sys.stdin)
assert x["authenticated"] is True, x
assert x["ready_match"] == "http://127.0.0.1:18081/ready", x
'

SESSION=$(curl -fsS \
  http://127.0.0.1:7331/_llmgateway/browser-sessions/fake-web \
  "${AUTH[@]}")
printf '%s' "$SESSION" | python3 -c '
import json,sys
x=json.load(sys.stdin)
assert x["status"] == "ready", x
assert x["last_ready_at"], x
assert x["last_verified_at"], x
'

STOP=$(curl -fsS -X POST \
  http://127.0.0.1:7331/_llmgateway/browser-sessions/fake-web/driver/stop \
  "${AUTH[@]}")
printf '%s' "$STOP" | python3 -c '
import json,sys
x=json.load(sys.stdin)
assert x["stopped"] is True, x
assert x["status"]["running"] is False, x
'
BROWSER_PID=""

echo "llmgateway Chromium driver + Browser Accounts UI smoke test passed"
