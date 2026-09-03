#!/usr/bin/env bash
set -euo pipefail

export LLMGATEWAY_API_KEY="ci-local-key"
export FAKE_API_KEY="fake-key"
export LLMGATEWAY_CONFIG="/tmp/llmgateway-browser-session-smoke.toml"
PROFILE_ROOT="/tmp/llmgateway-browser-profiles"

rm -rf "$PROFILE_ROOT"
rm -f data/llmgateway.db data/llmgateway.db-shm data/llmgateway.db-wal
mkdir -p data

cat >/tmp/llmgateway-browser-session-smoke.toml <<EOF
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
./target/debug/llmgateway >/tmp/llmgateway-browser-session.log 2>&1 &
PID=$!
trap 'kill "$PID" 2>/dev/null || true; rm -rf "$PROFILE_ROOT"; rm -f data/llmgateway.db data/llmgateway.db-shm data/llmgateway.db-wal /tmp/llmgateway-browser-session-smoke.toml' EXIT

for _ in {1..50}; do
  if curl -fsS http://127.0.0.1:7331/_llmgateway/health >/dev/null; then
    break
  fi
  sleep 0.2
done

AUTH=(-H "Authorization: Bearer ${LLMGATEWAY_API_KEY}")

LIST=$(curl -fsS http://127.0.0.1:7331/_llmgateway/browser-sessions "${AUTH[@]}")
printf '%s' "$LIST" | python3 -c '
import json,sys
x=json.load(sys.stdin)
assert x["enabled"] is True, x
assert len(x["sessions"]) == 1, x
s=x["sessions"][0]
assert s["id"] == "fake-web", s
assert s["status"] == "requires_login", s
assert "cookie" not in json.dumps(x).lower(), x
'

test -d "$PROFILE_ROOT/fake-web"
if [ "$(stat -c '%a' "$PROFILE_ROOT/fake-web")" != "700" ]; then
  echo "browser profile permissions are not 0700" >&2
  exit 1
fi

START=$(curl -fsS -X POST http://127.0.0.1:7331/_llmgateway/browser-sessions/fake-web/login/start "${AUTH[@]}")
printf '%s' "$START" | python3 -c '
import json,sys
x=json.load(sys.stdin)
assert x["session"]["status"] == "login_in_progress", x
assert x["login_attempt_id"].startswith("browser_login_"), x
assert x["login_url"].endswith("/login"), x
assert len(x["instructions"]) >= 4, x
assert "cookie" not in json.dumps(x).lower().replace("raw cookies", ""), x
'

READY=$(curl -fsS -X POST http://127.0.0.1:7331/_llmgateway/browser-sessions/fake-web/login/complete "${AUTH[@]}")
printf '%s' "$READY" | python3 -c '
import json,sys
x=json.load(sys.stdin)
assert x["ready"] is True, x
s=x["session"]
assert s["status"] == "ready", s
assert s["last_ready_at"], s
assert s["last_verified_at"], s
assert s["login_attempt_id"] is None, s
'

ATTN=$(curl -fsS -X POST http://127.0.0.1:7331/_llmgateway/browser-sessions/fake-web/attention \
  "${AUTH[@]}" -H "Content-Type: application/json" \
  -d '{"error":"session expired; user login required"}')
printf '%s' "$ATTN" | python3 -c '
import json,sys
x=json.load(sys.stdin)
s=x["session"]
assert x["requires_attention"] is True, x
assert s["status"] == "requires_attention", s
assert "session expired" in s["last_error"], s
'

RESET=$(curl -fsS -X POST http://127.0.0.1:7331/_llmgateway/browser-sessions/fake-web/reset "${AUTH[@]}")
printf '%s' "$RESET" | python3 -c '
import json,sys
x=json.load(sys.stdin)
s=x["session"]
assert x["reset"] is True, x
assert s["status"] == "requires_login", s
assert s["last_error"] is None, s
'

echo "llmgateway browser session lifecycle smoke test passed"
