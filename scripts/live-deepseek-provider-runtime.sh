#!/usr/bin/env bash
set -euo pipefail

BASE_URL="${LLMGATEWAY_BASE_URL:-http://127.0.0.1:7331}"
API_KEY="${LLMGATEWAY_API_KEY:-}"
ACCOUNT_ID=""
ROUTE_ID=""
KEEP_THREAD=0
TMP_DIR="$(mktemp -d)"
THREAD_ID=""

usage() {
  cat <<'EOF'
Usage:
  live-deepseek-provider-runtime.sh --account <id> [options]

Options:
  --base-url <url>   Gateway URL (default: http://127.0.0.1:7331)
  --api-key <key>    Or set LLMGATEWAY_API_KEY
  --route <route>    Explicit public route/model id
  --keep-thread
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --account) ACCOUNT_ID="${2:-}"; shift 2 ;;
    --base-url) BASE_URL="${2:-}"; shift 2 ;;
    --api-key) API_KEY="${2:-}"; shift 2 ;;
    --route) ROUTE_ID="${2:-}"; shift 2 ;;
    --keep-thread) KEEP_THREAD=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "Unknown argument: $1" >&2; usage >&2; exit 2 ;;
  esac
done

[[ -n "$ACCOUNT_ID" ]] || { echo "--account is required" >&2; exit 2; }
[[ -n "$API_KEY" ]] || { echo "--api-key is required or set LLMGATEWAY_API_KEY" >&2; exit 2; }
command -v curl >/dev/null || { echo "curl is required" >&2; exit 2; }
command -v python3 >/dev/null || { echo "python3 is required" >&2; exit 2; }
BASE_URL="${BASE_URL%/}"

cleanup() {
  if [[ "$KEEP_THREAD" -eq 0 && -n "$THREAD_ID" ]]; then
    curl -fsS -X DELETE -H "Authorization: Bearer $API_KEY"       "$BASE_URL/v1/threads/$THREAD_ID" >/dev/null 2>&1 || true
  fi
  rm -rf "$TMP_DIR"
}
trap cleanup EXIT

api() {
  local method="$1" path="$2" output="$3" body="${4:-}"
  local args=(-fsS -X "$method" -H "Authorization: Bearer $API_KEY")
  if [[ -n "$body" ]]; then
    args+=(-H "Content-Type: application/json" --data-binary "$body")
  fi
  curl "${args[@]}" "$BASE_URL$path" -o "$output"
}

runtime() {
  api GET "/_llmgateway/browser-accounts/$ACCOUNT_ID/runtime" "$1"
}

assert_direct_runtime() {
  local phase="$1" output="$TMP_DIR/runtime-$RANDOM.json"
  runtime "$output"
  python3 - "$output" "$phase" <<'PY'
import json,sys
x=json.load(open(sys.argv[1],encoding="utf-8"))
phase=sys.argv[2]
last=x.get("last_execution") or {}
if x.get("browser_running"):
    raise SystemExit(f"DEEPSEEK LIVE FAILED: Chromium is running after {phase}")
if last.get("transport") != "direct-http":
    raise SystemExit(f"DEEPSEEK LIVE FAILED: {phase} did not use direct-http: {last}")
if last.get("adapter_id") != "deepseek-web-http":
    raise SystemExit(f"DEEPSEEK LIVE FAILED: {phase} used unexpected adapter: {last}")
if last.get("browser_fallback"):
    raise SystemExit(f"DEEPSEEK LIVE FAILED: {phase} unexpectedly used browser fallback")
PY
}

api GET "/_llmgateway/health" "$TMP_DIR/health.json"
python3 - "$TMP_DIR/health.json" <<'PY'
import json,sys
x=json.load(open(sys.argv[1],encoding="utf-8"))
assert x.get("status") == "ok", x
PY

runtime "$TMP_DIR/preflight.json"
SESSION_ID="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1])).get("session_id") or "")' "$TMP_DIR/preflight.json")"
BROWSER_RUNNING="$(python3 -c 'import json,sys; print("1" if json.load(open(sys.argv[1])).get("browser_running") else "0")' "$TMP_DIR/preflight.json")"
if [[ "$BROWSER_RUNNING" == "1" && -n "$SESSION_ID" ]]; then
  api POST "/_llmgateway/browser-sessions/$SESSION_ID/driver/stop" "$TMP_DIR/stop.json"
  sleep 0.3
  runtime "$TMP_DIR/preflight.json"
fi
python3 - "$TMP_DIR/preflight.json" <<'PY'
import json,sys
x=json.load(open(sys.argv[1],encoding="utf-8"))
if x.get("browser_running"):
    raise SystemExit("DEEPSEEK LIVE FAILED: Chromium could not be stopped")
if not x.get("auth_snapshot_available"):
    raise SystemExit("DEEPSEEK LIVE FAILED: auth snapshot is unavailable")
if not x.get("direct_ready"):
    raise SystemExit(f"DEEPSEEK LIVE FAILED: account is not direct-ready: {x}")
adapter=x.get("adapter") or {}
if adapter.get("status") != "ready":
    raise SystemExit(f"DEEPSEEK LIVE FAILED: direct adapter is not ready: {adapter}")
PY

if [[ -z "$ROUTE_ID" ]]; then
  api GET "/v1/models" "$TMP_DIR/models.json"
  ROUTE_ID="$(python3 - "$TMP_DIR/models.json" "$ACCOUNT_ID" <<'PY'
import json,sys
payload=json.load(open(sys.argv[1],encoding="utf-8"))
account=sys.argv[2]
for model in payload.get("data",[]):
    meta=model.get("llmgateway") or {}
    if meta.get("kind") == "route" and meta.get("account") == account:
        print(model.get("id") or "")
        break
PY
)"
fi
[[ -n "$ROUTE_ID" ]] || { echo "DEEPSEEK LIVE FAILED: no route found for $ACCOUNT_ID" >&2; exit 1; }

CREATE="$(python3 - "$ROUTE_ID" <<'PY'
import json,sys
print(json.dumps({"title":"DeepSeek Provider Runtime live acceptance","model":sys.argv[1]}))
PY
)"
api POST "/v1/threads" "$TMP_DIR/thread.json" "$CREATE"
THREAD_ID="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["id"])' "$TMP_DIR/thread.json")"

send_buffered() {
  local prompt="$1" prefix="$2" body
  body="$(python3 - "$prompt" "$ROUTE_ID" <<'PY'
import json,sys
print(json.dumps({"content":sys.argv[1],"model":sys.argv[2],"stream":False}))
PY
)"
  curl -fsS -D "$TMP_DIR/$prefix.headers" -o "$TMP_DIR/$prefix.json"     -X POST -H "Authorization: Bearer $API_KEY" -H "Content-Type: application/json"     --data-binary "$body" "$BASE_URL/v1/threads/$THREAD_ID/messages"
  grep -qi "^x-llmgateway-route: $ROUTE_ID" "$TMP_DIR/$prefix.headers"
  python3 - "$TMP_DIR/$prefix.json" <<'PY'
import json,sys
x=json.load(open(sys.argv[1],encoding="utf-8"))
text=((x.get("choices") or [{}])[0].get("message") or {}).get("content")
if not str(text or "").strip():
    raise SystemExit("DEEPSEEK LIVE FAILED: buffered turn returned empty output")
PY
  assert_direct_runtime "$prefix"
}

send_buffered "Reply briefly with: deepseek-runtime-1" "turn1"
api GET "/_llmgateway/threads/$THREAD_ID/browser-affinity/$ACCOUNT_ID" "$TMP_DIR/affinity1.json"
URL1="$(python3 -c 'import json,sys; print((json.load(open(sys.argv[1])).get("mapping") or {}).get("conversation_url") or "")' "$TMP_DIR/affinity1.json")"
ORD1="$(python3 -c 'import json,sys; print(int((json.load(open(sys.argv[1])).get("mapping") or {}).get("last_synced_ordinal") or 0))' "$TMP_DIR/affinity1.json")"
[[ -n "$URL1" && "$ORD1" -gt 0 ]] || { echo "DEEPSEEK LIVE FAILED: native conversation was not persisted" >&2; exit 1; }

send_buffered "Reply briefly with: deepseek-runtime-2" "turn2"
api GET "/_llmgateway/threads/$THREAD_ID/browser-affinity/$ACCOUNT_ID" "$TMP_DIR/affinity2.json"
python3 - "$TMP_DIR/affinity2.json" "$URL1" "$ORD1" <<'PY'
import json,sys
x=json.load(open(sys.argv[1],encoding="utf-8"))
mapping=x.get("mapping") or {}
if mapping.get("conversation_url") != sys.argv[2]:
    raise SystemExit(f"DEEPSEEK LIVE FAILED: same thread changed native conversation: {mapping}")
if int(mapping.get("last_synced_ordinal") or 0) <= int(sys.argv[3]):
    raise SystemExit(f"DEEPSEEK LIVE FAILED: sync ordinal did not advance: {mapping}")
PY

STREAM_BODY="$(python3 - "$ROUTE_ID" <<'PY'
import json,sys
print(json.dumps({"content":"Reply briefly with: deepseek-runtime-stream","model":sys.argv[1],"stream":True}))
PY
)"
curl -fsS --no-buffer -D "$TMP_DIR/stream.headers" -o "$TMP_DIR/stream.sse"   -X POST -H "Authorization: Bearer $API_KEY" -H "Content-Type: application/json"   --data-binary "$STREAM_BODY" "$BASE_URL/v1/threads/$THREAD_ID/messages"
grep -qi "^x-llmgateway-route: $ROUTE_ID" "$TMP_DIR/stream.headers"
grep -Eq '^data:[[:space:]]*\[DONE\][[:space:]]*$' "$TMP_DIR/stream.sse"
grep -Fq '"content"' "$TMP_DIR/stream.sse"
grep -Eq '"finish_reason"[[:space:]]*:[[:space:]]*"[^"]+"' "$TMP_DIR/stream.sse"
assert_direct_runtime "stream"

echo
echo "DEEPSEEK PROVIDER RUNTIME LIVE ACCEPTANCE: PASS"
echo "Account: $ACCOUNT_ID"
echo "Route:   $ROUTE_ID"
echo "Native conversation: $URL1"
echo "Chromium running: false"
