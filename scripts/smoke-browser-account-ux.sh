#!/usr/bin/env bash
set -euo pipefail

export LLMGATEWAY_API_KEY="ci-local-key"
export FAKE_API_KEY="fake-key"
export LLMGATEWAY_CONFIG="/tmp/llmgateway-browser-account-ux-smoke.toml"
PROFILE_ROOT="/tmp/llmgateway-browser-account-ux-profiles"
FAKE_CHROMIUM="/tmp/llmgateway-fake-cdp-chromium"
BROWSER_PID=""

rm -rf "$PROFILE_ROOT"
rm -f data/llmgateway.db data/llmgateway.db-shm data/llmgateway.db-wal
mkdir -p data

cat >"$FAKE_CHROMIUM" <<'SH'
#!/usr/bin/env bash
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
[browser]
enabled = false
profile_root = "$PROFILE_ROOT"
[chromium]
enabled = false
executable = "$FAKE_CHROMIUM"
startup_timeout_seconds = 5
auto_recover = true
reconcile_interval_seconds = 15
extra_args = []
[context]
enabled = false
retrieval_enabled = false

[[providers]]
id = "fake-api"
kind = "openai-compatible"
base_url = "http://127.0.0.1:18080/v1"
models_path = "models"
[[accounts]]
id = "api-account"
provider = "fake-api"
api_key_env = "FAKE_API_KEY"
enabled = true
discover_models = false
[[routes]]
id = "api-route"
account = "api-account"
model = "fake-model"
priority = 20
enabled = true
capabilities = ["chat"]
[virtual_models.llmgateway-auto]
routes = ["api-route"]
[virtual_models.llmgateway-coding]
routes = ["api-route"]
[virtual_models.llmgateway-best]
routes = ["api-route"]
EOF

python3 scripts/fake-openai.py >/tmp/llmgateway-browser-account-ux-api.log 2>&1 &
FAKE_PID=$!
cargo build --quiet
./target/debug/llmgateway >/tmp/llmgateway-browser-account-ux.log 2>&1 &
PID=$!

cleanup() {
  if [ -n "$BROWSER_PID" ]; then kill "$BROWSER_PID" 2>/dev/null || true; fi
  kill "$PID" "$FAKE_PID" 2>/dev/null || true
  rm -rf "$PROFILE_ROOT"
  rm -f "$FAKE_CHROMIUM" "$LLMGATEWAY_CONFIG"
  rm -f data/llmgateway.db data/llmgateway.db-shm data/llmgateway.db-wal
}
trap cleanup EXIT

for _ in {1..60}; do
  curl -fsS http://127.0.0.1:7331/_llmgateway/health >/dev/null && break
  sleep 0.2
done

AUTH=(-H "Authorization: Bearer ${LLMGATEWAY_API_KEY}")
JSON=(-H "Content-Type: application/json")

MISSING_BODY=/tmp/llmgateway-browser-account-missing.json
MISSING_STATUS=$(curl -sS -o "$MISSING_BODY" -w '%{http_code}' -X PATCH \
  http://127.0.0.1:7331/_llmgateway/browser-account-setup/nonexistent-account \
  "${AUTH[@]}" "${JSON[@]}" -d '{"enabled":false}')
test "$MISSING_STATUS" = "404"
python3 - "$MISSING_BODY" <<'PY'
import json,sys
with open(sys.argv[1], encoding="utf-8") as f:
    payload=json.load(f)
assert payload["error"]["type"] == "not_found_error", payload
PY

PRESETS=$(curl -fsS http://127.0.0.1:7331/_llmgateway/browser-account-setup/providers "${AUTH[@]}")
printf '%s' "$PRESETS" | python3 -c '
import json,sys
x=json.load(sys.stdin)
assert x["hot_activation"] is True, x
assert x["restart_required_after_create"] is False, x
assert {p["id"] for p in x["providers"]} >= {"gemini","chatgpt","qwen"}, x
'

BEFORE=$(curl -fsS http://127.0.0.1:7331/_llmgateway/browser-sessions "${AUTH[@]}")
printf '%s' "$BEFORE" | python3 -c 'import json,sys; x=json.load(sys.stdin); assert x["sessions"] == [], x'

CREATE=$(curl -fsS -X POST http://127.0.0.1:7331/_llmgateway/browser-account-setup   "${AUTH[@]}" "${JSON[@]}"   -d '{"provider":"qwen","account_id":"qwen-ci","label":"Qwen CI","priority":5}')
printf '%s' "$CREATE" | python3 -c '
import json,sys
x=json.load(sys.stdin)
assert x["account_id"] == "qwen-ci", x
assert x["session_id"] == "qwen-ci", x
assert x["route_id"] == "qwen-ci-route", x
assert x["model_id"] == "qwen-web-default", x
assert x["restart_required"] is False, x
'

python3 - "$LLMGATEWAY_CONFIG" <<'PY'
import sys, tomllib
with open(sys.argv[1], "rb") as f: x=tomllib.load(f)
account=next(a for a in x["accounts"] if a["id"] == "qwen-ci")
route=next(r for r in x["routes"] if r["id"] == "qwen-ci-route")
assert account["provider"] == "qwen-web" and account["enabled"] is True, account
assert route["account"] == "qwen-ci" and route["model"] == "qwen-web-default", route
assert x["browser"]["bindings"]["qwen-ci"]["session"] == "qwen-ci", x["browser"]
assert x["chromium"]["sessions"]["qwen-ci"]["enabled"] is True, x["chromium"]
PY

TRANSPORT_BEFORE=$(curl -fsS http://127.0.0.1:7331/_llmgateway/accounts/qwen-ci/transport "${AUTH[@]}")
printf '%s' "$TRANSPORT_BEFORE" | python3 -c '
import json,sys
x=json.load(sys.stdin)
assert x["desired_policy"] == "browser-only", x
assert x["configured_mode"] == "auto", x
assert x["browserless"]["supported"] is True, x
assert x["browserless"]["recommended_mode"] == "http-preferred", x
assert x["browserless"]["supports_direct_model_discovery"] is True, x
assert x["browserless"]["supports_native_conversation"] is True, x
assert x["auth_state"] == "unavailable", x
'

TRANSPORT_ON=$(curl -fsS -X PATCH \
  http://127.0.0.1:7331/_llmgateway/accounts/qwen-ci/transport \
  "${AUTH[@]}" "${JSON[@]}" -d '{"transport_policy":"browserless-preferred"}')
printf '%s' "$TRANSPORT_ON" | python3 -c '
import json,sys
x=json.load(sys.stdin)
assert x["desired_policy"] == "browserless-preferred", x
assert x["configured_mode"] == "http-preferred", x
assert x["browserless"]["recommended_mode"] == "http-preferred", x
assert x["effective_transport"] == "unavailable", x
'

python3 - "$LLMGATEWAY_CONFIG" <<'PY'
import sys,tomllib
with open(sys.argv[1], "rb") as f: x=tomllib.load(f)
assert x["browser"]["bindings"]["qwen-ci"]["transport_mode"] == "http-preferred", x["browser"]["bindings"]["qwen-ci"]
PY

kill "$PID" 2>/dev/null || true
wait "$PID" 2>/dev/null || true
./target/debug/llmgateway >/tmp/llmgateway-browser-account-ux.log 2>&1 &
PID=$!
for _ in {1..60}; do
  curl -fsS http://127.0.0.1:7331/_llmgateway/health >/dev/null && break
  sleep 0.2
done

TRANSPORT_AFTER_ON_RESTART=$(curl -fsS http://127.0.0.1:7331/_llmgateway/accounts/qwen-ci/transport "${AUTH[@]}")
printf '%s' "$TRANSPORT_AFTER_ON_RESTART" | python3 -c '
import json,sys
x=json.load(sys.stdin)
assert x["desired_policy"] == "browserless-preferred", x
assert x["configured_mode"] == "http-preferred", x
'

TRANSPORT_OFF=$(curl -fsS -X PATCH \
  http://127.0.0.1:7331/_llmgateway/accounts/qwen-ci/transport \
  "${AUTH[@]}" "${JSON[@]}" -d '{"transport_policy":"browser-only"}')
printf '%s' "$TRANSPORT_OFF" | python3 -c '
import json,sys
x=json.load(sys.stdin)
assert x["desired_policy"] == "browser-only", x
assert x["configured_mode"] == "browser-only", x
'

kill "$PID" 2>/dev/null || true
wait "$PID" 2>/dev/null || true
./target/debug/llmgateway >/tmp/llmgateway-browser-account-ux.log 2>&1 &
PID=$!
for _ in {1..60}; do
  curl -fsS http://127.0.0.1:7331/_llmgateway/health >/dev/null && break
  sleep 0.2
done

TRANSPORT_AFTER_OFF_RESTART=$(curl -fsS http://127.0.0.1:7331/_llmgateway/accounts/qwen-ci/transport "${AUTH[@]}")
printf '%s' "$TRANSPORT_AFTER_OFF_RESTART" | python3 -c '
import json,sys
x=json.load(sys.stdin)
assert x["desired_policy"] == "browser-only", x
assert x["configured_mode"] == "browser-only", x
'

ACCOUNTS=$(curl -fsS http://127.0.0.1:7331/_llmgateway/accounts "${AUTH[@]}")
printf '%s' "$ACCOUNTS" | python3 -c '
import json,sys
a=next(a for a in json.load(sys.stdin)["data"] if a["id"] == "qwen-ci")
assert a["enabled"] is True and a["provider"] == "qwen-web", a
'

SESSIONS=$(curl -fsS http://127.0.0.1:7331/_llmgateway/browser-sessions "${AUTH[@]}")
printf '%s' "$SESSIONS" | python3 -c '
import json,sys
s=next(s for s in json.load(sys.stdin)["sessions"] if s["id"] == "qwen-ci")
assert s["status"] == "login_required", s
'

CATALOG=$(curl -fsS http://127.0.0.1:7331/_llmgateway/models "${AUTH[@]}")
printf '%s' "$CATALOG" | python3 -c '
import json,sys
m=next(m for m in json.load(sys.stdin)["data"] if m["id"] == "qwen-web/qwen-web-default")
assert any(a["account_id"] == "qwen-ci" for a in m["accounts"]), m
assert "qwen-ci-route" in m["routes"], m
'

PUBLIC_MODELS=$(curl -fsS http://127.0.0.1:7331/v1/models "${AUTH[@]}")
printf '%s' "$PUBLIC_MODELS" | python3 -c '
import json,sys
ids={m["id"] for m in json.load(sys.stdin)["data"]}
assert "qwen-ci-route" in ids, ids
assert "qwen-web/qwen-web-default" in ids, ids
'

BEFORE_ROUTE=$(curl -fsS -X POST http://127.0.0.1:7331/_llmgateway/routes/explain   "${AUTH[@]}" "${JSON[@]}"   -d '{"model":"llmgateway-auto","body":{"messages":[{"role":"user","content":"before login"}]}}')
printf '%s' "$BEFORE_ROUTE" | python3 -c '
import json,sys
x=json.load(sys.stdin)
assert x["selected_route"] == "api-route", x
b=next(c for c in x["candidates"] if c["route_id"] == "qwen-ci-route")
assert b["eligible"] is False, b
assert any("browser_" in r for r in b["exclusion_reasons"]), b
'

DUP_BODY=/tmp/llmgateway-browser-account-ux-duplicate.json
DUP_STATUS=$(curl -sS -o "$DUP_BODY" -w '%{http_code}' -X POST   http://127.0.0.1:7331/_llmgateway/browser-account-setup   "${AUTH[@]}" "${JSON[@]}" -d '{"provider":"qwen","account_id":"qwen-ci"}')
test "$DUP_STATUS" = "409"

LAUNCH=$(curl -fsS -X POST   http://127.0.0.1:7331/_llmgateway/browser-sessions/qwen-ci/driver/launch "${AUTH[@]}")
BROWSER_PID=$(printf '%s' "$LAUNCH" | python3 -c 'import json,sys; print(json.load(sys.stdin)["launch"]["pid"] or "")')
printf '%s' "$LAUNCH" | python3 -c '
import json,sys
x=json.load(sys.stdin)
assert x["launched"] is True, x
assert x["launch"]["debugger_port"] > 0, x
'

VERIFY=$(curl -fsS -X POST   http://127.0.0.1:7331/_llmgateway/browser-sessions/qwen-ci/driver/verify "${AUTH[@]}")
printf '%s' "$VERIFY" | python3 -c '
import json,sys
x=json.load(sys.stdin)
assert x["authenticated"] is True, x
assert x["ready_match"] == "https://chat.qwen.ai/", x
'

READY_ROUTE=$(curl -fsS -X POST http://127.0.0.1:7331/_llmgateway/routes/explain   "${AUTH[@]}" "${JSON[@]}"   -d '{"model":"llmgateway-auto","body":{"messages":[{"role":"user","content":"after login"}]}}')
printf '%s' "$READY_ROUTE" | python3 -c '
import json,sys
x=json.load(sys.stdin)
assert x["selected_route"] == "qwen-ci-route", x
b=next(c for c in x["candidates"] if c["route_id"] == "qwen-ci-route")
assert b["eligible"] is True, b
assert b["readiness"]["browser_adapter_status"] == "ready", b
'

curl -fsS -D /tmp/llmgateway-browser-account-ux-browser.headers   -o /tmp/llmgateway-browser-account-ux-browser.json   -X POST http://127.0.0.1:7331/v1/chat/completions   "${AUTH[@]}" "${JSON[@]}"   -d '{"model":"llmgateway-auto","stream":false,"messages":[{"role":"user","content":"browser hot route"}]}'
grep -qi '^x-llmgateway-route: qwen-ci-route' /tmp/llmgateway-browser-account-ux-browser.headers
python3 - /tmp/llmgateway-browser-account-ux-browser.json <<'PY'
import json,sys
with open(sys.argv[1], encoding="utf-8") as f: x=json.load(f)
assert x["choices"][0]["message"]["content"] == "browser-hot-ok", x
PY

DISABLE=$(curl -fsS -X PATCH   http://127.0.0.1:7331/_llmgateway/browser-account-setup/qwen-ci   "${AUTH[@]}" "${JSON[@]}" -d '{"enabled":false}')
printf '%s' "$DISABLE" | python3 -c 'import json,sys; x=json.load(sys.stdin); assert x["enabled"] is False and x["restart_required"] is False, x'

curl -fsS -D /tmp/llmgateway-browser-account-ux-api.headers   -o /tmp/llmgateway-browser-account-ux-api.json   -X POST http://127.0.0.1:7331/v1/chat/completions   "${AUTH[@]}" "${JSON[@]}"   -d '{"model":"llmgateway-auto","stream":false,"messages":[{"role":"user","content":"fallback after disable"}]}'
grep -qi '^x-llmgateway-route: api-route' /tmp/llmgateway-browser-account-ux-api.headers

ENABLE=$(curl -fsS -X PATCH   http://127.0.0.1:7331/_llmgateway/browser-account-setup/qwen-ci   "${AUTH[@]}" "${JSON[@]}" -d '{"enabled":true}')
printf '%s' "$ENABLE" | python3 -c 'import json,sys; x=json.load(sys.stdin); assert x["enabled"] is True and x["restart_required"] is False, x'

REENABLED=$(curl -fsS -X POST http://127.0.0.1:7331/_llmgateway/routes/explain   "${AUTH[@]}" "${JSON[@]}"   -d '{"model":"llmgateway-auto","body":{"messages":[{"role":"user","content":"re-enabled"}]}}')
printf '%s' "$REENABLED" | python3 -c 'import json,sys; x=json.load(sys.stdin); assert x["selected_route"] == "qwen-ci-route", x'

curl -fsS -X POST   http://127.0.0.1:7331/_llmgateway/browser-sessions/qwen-ci/driver/stop "${AUTH[@]}" >/dev/null
BROWSER_PID=""

echo "llmgateway v0.29 browser account hot activation + lifecycle E2E smoke test passed"
