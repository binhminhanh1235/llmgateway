#!/usr/bin/env bash
set -euo pipefail

BASE_URL="${LLMGATEWAY_BASE_URL:-http://127.0.0.1:7331}"
API_KEY="${LLMGATEWAY_API_KEY:-}"
ACCOUNT_ID=""
ROUTE_ID=""
KEEP_THREADS=0
SKIP_STREAM=0
TEST_CANCELLATION=0

usage() {
  cat <<'EOF'
Usage:
  live-browserless-acceptance.sh --account <id> [options]

Options:
  --base-url <url>       Gateway URL (default: http://127.0.0.1:7331)
  --api-key <key>        Gateway API key (or set LLMGATEWAY_API_KEY)
  --route <route-id>     Force a route instead of auto-resolving by account
  --keep-threads         Keep generated acceptance threads
  --skip-stream          Skip the streaming scenario
  --test-cancellation    Try a client-aborted streaming scenario
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --account) ACCOUNT_ID="${2:-}"; shift 2 ;;
    --base-url) BASE_URL="${2:-}"; shift 2 ;;
    --api-key) API_KEY="${2:-}"; shift 2 ;;
    --route) ROUTE_ID="${2:-}"; shift 2 ;;
    --keep-threads) KEEP_THREADS=1; shift ;;
    --skip-stream) SKIP_STREAM=1; shift ;;
    --test-cancellation) TEST_CANCELLATION=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "Unknown argument: $1" >&2; usage >&2; exit 2 ;;
  esac
done

if [[ -z "$ACCOUNT_ID" ]]; then
  echo "--account is required" >&2
  exit 2
fi
if [[ -z "$API_KEY" ]]; then
  echo "--api-key is required or set LLMGATEWAY_API_KEY" >&2
  exit 2
fi
command -v curl >/dev/null || { echo "curl is required" >&2; exit 2; }
command -v python3 >/dev/null || { echo "python3 is required" >&2; exit 2; }

BASE_URL="${BASE_URL%/}"
TMP_DIR="$(mktemp -d)"
THREADS=()

cleanup() {
  if [[ "$KEEP_THREADS" -eq 0 ]]; then
    for thread in "${THREADS[@]:-}"; do
      [[ -n "$thread" ]] || continue
      curl -fsS -X DELETE \
        -H "Authorization: Bearer $API_KEY" \
        "$BASE_URL/v1/threads/$thread" >/dev/null 2>&1 || true
    done
  else
    echo "[browserless-live] Keeping acceptance threads: ${THREADS[*]:-none}"
  fi
  rm -rf "$TMP_DIR"
}
trap cleanup EXIT

step() {
  echo "[browserless-live] $*"
}

api() {
  local method="$1"
  local path="$2"
  local output="$3"
  local body="${4:-}"
  local args=(-fsS -X "$method" -H "Authorization: Bearer $API_KEY")
  if [[ -n "$body" ]]; then
    args+=(-H "Content-Type: application/json" --data-binary "$body")
  fi
  curl "${args[@]}" "$BASE_URL$path" -o "$output"
}

json_eval() {
  local file="$1"
  local expr="$2"
  python3 - "$file" "$expr" <<'PY'
import json, sys
path, expr = sys.argv[1], sys.argv[2]
with open(path, encoding="utf-8") as f:
    value = json.load(f)
safe = {"bool": bool, "int": int, "str": str, "any": any, "len": len}
result = eval(expr, {"__builtins__": {}}, {"x": value, **safe})
if isinstance(result, bool):
    print("true" if result else "false")
elif result is None:
    print("")
else:
    print(result)
PY
}

assert_python() {
  local file="$1"
  local expression="$2"
  local message="$3"
  python3 - "$file" "$expression" "$message" <<'PY'
import json, sys
path, expr, message = sys.argv[1:4]
with open(path, encoding="utf-8") as f:
    x = json.load(f)
safe = {"bool": bool, "int": int, "str": str, "any": any, "len": len}
if not eval(expr, {"__builtins__": {}}, {"x": x, **safe}):
    raise SystemExit("ACCEPTANCE FAILED: " + message + "\n" + json.dumps(x, indent=2))
PY
}

runtime_file() {
  local output="$1"
  api GET "/_llmgateway/browser-accounts/$ACCOUNT_ID/runtime" "$output"
}

affinity_file() {
  local thread="$1"
  local output="$2"
  api GET "/_llmgateway/threads/$thread/browser-affinity/$ACCOUNT_ID" "$output"
}

assert_browser_closed() {
  local phase="$1"
  local output="$TMP_DIR/runtime-after.json"
  sleep 0.25
  runtime_file "$output"
  assert_python "$output" "not bool(x.get('browser_running'))" "Chromium is still running after $phase"
}

assert_direct_execution() {
  local phase="$1"
  local output="$TMP_DIR/runtime-direct.json"
  assert_browser_closed "$phase"
  runtime_file "$output"
  assert_python "$output" "x.get('last_execution') is not None" "no execution transport telemetry after $phase"
  assert_python "$output" "x.get('last_execution',{}).get('transport') == 'direct-http'" "turn did not use direct-http after $phase"
  assert_python "$output" "not bool(x.get('last_execution',{}).get('browser_fallback'))" "turn used browser fallback after $phase"
  assert_python "$output" "x.get('last_execution',{}).get('adapter_id') in ['gemini-web-http','chatgpt-web-http']" "turn used an unexpected adapter after $phase"
}

create_thread() {
  local title="$1"
  local output="$TMP_DIR/thread-create.json"
  local body
  body="$(python3 - "$title" "$ROUTE_ID" <<'PY'
import json, sys
print(json.dumps({"title": sys.argv[1], "model": sys.argv[2]}))
PY
)"
  api POST "/v1/threads" "$output" "$body"
  LAST_THREAD_ID="$(json_eval "$output" "x.get('id','')")"
  [[ -n "$LAST_THREAD_ID" ]] || { echo "ACCEPTANCE FAILED: thread creation returned no id" >&2; exit 1; }
  THREADS+=("$LAST_THREAD_ID")
}

send_message() {
  local thread="$1"
  local prompt="$2"
  local stream="$3"
  local output="$4"
  local headers="$5"
  local body
  body="$(python3 - "$prompt" "$ROUTE_ID" "$stream" <<'PY'
import json, sys
print(json.dumps({
    "content": sys.argv[1],
    "model": sys.argv[2],
    "stream": sys.argv[3].lower() == "true"
}))
PY
)"
  curl -fsS -D "$headers" -o "$output" \
    -X POST \
    -H "Authorization: Bearer $API_KEY" \
    -H "Content-Type: application/json" \
    --data-binary "$body" \
    "$BASE_URL/v1/threads/$thread/messages"
  local routed
  routed="$(python3 - "$headers" <<'PY'
import sys
for line in open(sys.argv[1], encoding="utf-8", errors="replace"):
    if line.lower().startswith("x-llmgateway-route:"):
        print(line.split(":", 1)[1].strip())
        break
PY
)"
  [[ "$routed" == "$ROUTE_ID" ]] || {
    echo "ACCEPTANCE FAILED: request routed through '$routed' instead of '$ROUTE_ID'" >&2
    exit 1
  }
}

step "Checking gateway health"
api GET "/_llmgateway/health" "$TMP_DIR/health.json"
assert_python "$TMP_DIR/health.json" "x.get('status') == 'ok'" "gateway health is not ok"

step "Resolving route for account '$ACCOUNT_ID'"
if [[ -z "$ROUTE_ID" ]]; then
  api GET "/v1/models" "$TMP_DIR/models.json"
  ROUTE_ID="$(python3 - "$TMP_DIR/models.json" "$ACCOUNT_ID" <<'PY'
import json, sys
with open(sys.argv[1], encoding="utf-8") as f:
    payload = json.load(f)
account = sys.argv[2]
for model in payload.get("data", []):
    meta = model.get("llmgateway") or {}
    if meta.get("kind") == "route" and meta.get("account") == account:
        print(model.get("id", ""))
        break
PY
)"
fi
[[ -n "$ROUTE_ID" ]] || { echo "ACCEPTANCE FAILED: no route found for '$ACCOUNT_ID'" >&2; exit 1; }
step "Using route '$ROUTE_ID'"

step "Checking direct transport readiness"
runtime_file "$TMP_DIR/runtime.json"
if [[ "$(json_eval "$TMP_DIR/runtime.json" "bool(x.get('browser_running'))")" == "true" ]]; then
  SESSION_ID="$(json_eval "$TMP_DIR/runtime.json" "x.get('session_id','')")"
  step "Chromium is running; stopping it before browserless acceptance"
  api POST "/_llmgateway/browser-sessions/$SESSION_ID/driver/stop" "$TMP_DIR/stop.json"
  assert_browser_closed "initial stop"
  runtime_file "$TMP_DIR/runtime.json"
fi
assert_python "$TMP_DIR/runtime.json" "bool(x.get('auth_snapshot_available'))" "saved browser auth snapshot is unavailable"
assert_python "$TMP_DIR/runtime.json" "x.get('adapter',{}).get('status') == 'ready'" "direct adapter is not ready"
assert_python "$TMP_DIR/runtime.json" "bool(x.get('direct_ready'))" "account is not direct-ready"
assert_python "$TMP_DIR/runtime.json" "x.get('effective_transport') == 'direct-http'" "effective transport is not direct-http"

step "Scenario 1/4: fresh native conversation"
create_thread "Browserless acceptance A"
THREAD_A="$LAST_THREAD_ID"
send_message "$THREAD_A" "Reply with exactly: browserless-a1" false "$TMP_DIR/a1.json" "$TMP_DIR/a1.headers"
assert_python "$TMP_DIR/a1.json" "bool((x.get('choices') or [{}])[0].get('message',{}).get('content'))" "fresh thread returned empty assistant content"
assert_direct_execution "fresh thread"
affinity_file "$THREAD_A" "$TMP_DIR/affinity-a1.json"
assert_python "$TMP_DIR/affinity-a1.json" "x.get('mapping') is not None and bool(x['mapping'].get('conversation_url'))" "fresh thread has no native mapping"
assert_python "$TMP_DIR/affinity-a1.json" "int(x['mapping'].get('last_synced_ordinal',0)) > 0" "fresh thread mapping was not synced"
URL_A="$(json_eval "$TMP_DIR/affinity-a1.json" "x['mapping']['conversation_url']")"
ORD_A1="$(json_eval "$TMP_DIR/affinity-a1.json" "int(x['mapping']['last_synced_ordinal'])")"

step "Scenario 2/4: same local thread keeps the same native conversation"
send_message "$THREAD_A" "Reply with exactly: browserless-a2" false "$TMP_DIR/a2.json" "$TMP_DIR/a2.headers"
assert_python "$TMP_DIR/a2.json" "bool((x.get('choices') or [{}])[0].get('message',{}).get('content'))" "second turn returned empty assistant content"
assert_direct_execution "same-thread second turn"
affinity_file "$THREAD_A" "$TMP_DIR/affinity-a2.json"
assert_python "$TMP_DIR/affinity-a2.json" "x.get('mapping',{}).get('conversation_url') == '$URL_A'" "same thread switched native conversation"
assert_python "$TMP_DIR/affinity-a2.json" "int(x.get('mapping',{}).get('last_synced_ordinal',0)) > $ORD_A1" "same-thread sync ordinal did not advance"

step "Scenario 3/4: a new local thread gets a different native conversation"
create_thread "Browserless acceptance B"
THREAD_B="$LAST_THREAD_ID"
send_message "$THREAD_B" "Reply with exactly: browserless-b1" false "$TMP_DIR/b1.json" "$TMP_DIR/b1.headers"
assert_python "$TMP_DIR/b1.json" "bool((x.get('choices') or [{}])[0].get('message',{}).get('content'))" "second local thread returned empty assistant content"
assert_direct_execution "second local thread"
affinity_file "$THREAD_B" "$TMP_DIR/affinity-b1.json"
URL_B="$(json_eval "$TMP_DIR/affinity-b1.json" "x.get('mapping',{}).get('conversation_url','')")"
[[ -n "$URL_B" && "$URL_B" != "$URL_A" ]] || {
  echo "ACCEPTANCE FAILED: two local threads mapped to the same native conversation" >&2
  exit 1
}

if [[ "$SKIP_STREAM" -eq 0 ]]; then
  step "Scenario 4/4: streaming completes and preserves native affinity"
  affinity_file "$THREAD_A" "$TMP_DIR/affinity-before-stream.json"
  ORD_STREAM="$(json_eval "$TMP_DIR/affinity-before-stream.json" "int(x.get('mapping',{}).get('last_synced_ordinal',0))")"
  send_message "$THREAD_A" "Reply with exactly: browserless-stream" true "$TMP_DIR/stream.sse" "$TMP_DIR/stream.headers"
  grep -Fq 'data: [DONE]' "$TMP_DIR/stream.sse" || { echo "ACCEPTANCE FAILED: stream ended without [DONE]" >&2; exit 1; }
  grep -Fq '"content"' "$TMP_DIR/stream.sse" || { echo "ACCEPTANCE FAILED: stream returned no assistant delta" >&2; exit 1; }
  grep -Eq '"finish_reason"[[:space:]]*:[[:space:]]*"[^"]+"' "$TMP_DIR/stream.sse" || { echo "ACCEPTANCE FAILED: stream returned no terminal finish_reason" >&2; exit 1; }
  STREAM_FRAMES="$(grep -Ec '^data:[[:space:]]*\{' "$TMP_DIR/stream.sse" || true)"
  [[ "$STREAM_FRAMES" -ge 2 ]] || { echo "ACCEPTANCE FAILED: stream did not expose incremental SSE frames" >&2; exit 1; }
  assert_direct_execution "streaming turn"
  sleep 0.3
  affinity_file "$THREAD_A" "$TMP_DIR/affinity-after-stream.json"
  assert_python "$TMP_DIR/affinity-after-stream.json" "x.get('mapping',{}).get('conversation_url') == '$URL_A'" "streaming turn changed native conversation"
  assert_python "$TMP_DIR/affinity-after-stream.json" "int(x.get('mapping',{}).get('last_synced_ordinal',0)) > $ORD_STREAM" "streaming sync ordinal did not advance"
  api GET "/v1/threads/$THREAD_A" "$TMP_DIR/thread-after-stream.json"
  assert_python "$TMP_DIR/thread-after-stream.json" "not any(m.get('role') == 'assistant' and not str(m.get('content') or '').strip() for m in x.get('messages',[]))" "streaming persisted an empty/stale assistant message"
fi

if [[ "$TEST_CANCELLATION" -eq 1 ]]; then
  step "Optional cancellation scenario: aborting a streaming request"
  create_thread "Browserless cancellation"
THREAD_C="$LAST_THREAD_ID"
  CANCEL_BODY="$(python3 - "$ROUTE_ID" <<'PY'
import json, sys
print(json.dumps({
    "content": "Write a detailed response with at least 800 words. Begin with cancellation-test.",
    "model": sys.argv[1],
    "stream": True
}))
PY
)"
  set +e
  curl -sS --no-buffer --max-time 1 \
    -H "Authorization: Bearer $API_KEY" \
    -H "Content-Type: application/json" \
    --data-binary "$CANCEL_BODY" \
    "$BASE_URL/v1/threads/$THREAD_C/messages" >/dev/null
  CURL_EXIT=$?
  set -e
  if [[ "$CURL_EXIT" -eq 28 ]]; then
    sleep 2
    assert_browser_closed "cancelled stream"
    api GET "/v1/threads/$THREAD_C" "$TMP_DIR/cancel-thread.json"
    assert_python "$TMP_DIR/cancel-thread.json" "not any(m.get('role') == 'assistant' for m in x.get('messages',[]))" "cancelled stream persisted an assistant message"
  else
    echo "[browserless-live] WARNING: cancellation request finished before timeout; cleanup was not exercised."
  fi
fi

runtime_file "$TMP_DIR/final-runtime.json"
assert_python "$TMP_DIR/final-runtime.json" "bool(x.get('direct_ready')) and x.get('effective_transport') == 'direct-http'" "account is no longer direct-ready"
assert_python "$TMP_DIR/final-runtime.json" "not bool(x.get('browser_running'))" "Chromium is running at the end of acceptance"

echo
echo "BROWSERLESS LIVE ACCEPTANCE: PASS"
echo "Account: $ACCOUNT_ID"
echo "Route:   $ROUTE_ID"
echo "Thread A native: $URL_A"
echo "Thread B native: $URL_B"
echo "Chromium running: false"
