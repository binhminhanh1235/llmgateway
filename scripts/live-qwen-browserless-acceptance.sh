#!/usr/bin/env bash
set -euo pipefail

BASE_URL="${LLMGATEWAY_BASE_URL:-http://127.0.0.1:7331}"
API_KEY="${LLMGATEWAY_API_KEY:-}"
ACCOUNT_ID=""
MODEL_A=""
MODEL_B=""
KEEP_THREADS=0
TEST_CANCELLATION=0
THREADS=()
TMP_DIR="$(mktemp -d)"

usage() {
  cat <<'EOF'
Usage:
  live-qwen-browserless-acceptance.sh --account <id> [options]

Options:
  --base-url <url>
  --api-key <key>       Or set LLMGATEWAY_API_KEY
  --model-a <id>        Canonical or external discovered Qwen model
  --model-b <id>        Canonical or external discovered Qwen model
  --test-cancellation   Abort one stream and verify no assistant is persisted
  --keep-threads
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --account) ACCOUNT_ID="${2:-}"; shift 2 ;;
    --base-url) BASE_URL="${2:-}"; shift 2 ;;
    --api-key) API_KEY="${2:-}"; shift 2 ;;
    --model-a) MODEL_A="${2:-}"; shift 2 ;;
    --model-b) MODEL_B="${2:-}"; shift 2 ;;
    --test-cancellation) TEST_CANCELLATION=1; shift ;;
    --keep-threads) KEEP_THREADS=1; shift ;;
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
  if [[ "$KEEP_THREADS" -eq 0 ]]; then
    for thread in "${THREADS[@]:-}"; do
      [[ -n "$thread" ]] || continue
      curl -fsS -X DELETE -H "Authorization: Bearer $API_KEY" \
        "$BASE_URL/v1/threads/$thread" >/dev/null 2>&1 || true
    done
  else
    echo "[qwen-browserless-live] Keeping threads: ${THREADS[*]:-none}"
  fi
  rm -rf "$TMP_DIR"
}
trap cleanup EXIT

step() { echo "[qwen-browserless-live] $*"; }

api() {
  local method="$1" path="$2" output="$3" body="${4:-}"
  local args=(-fsS -X "$method" -H "Authorization: Bearer $API_KEY")
  if [[ -n "$body" ]]; then
    args+=(-H "Content-Type: application/json" --data-binary "$body")
  fi
  curl "${args[@]}" "$BASE_URL$path" -o "$output"
}

assert_browser_stopped() {
  local phase="$1" output="$TMP_DIR/runtime-$RANDOM.json"
  api GET "/_llmgateway/browser-accounts/$ACCOUNT_ID/runtime" "$output"
  python3 - "$output" "$phase" <<'PY'
import json,sys
runtime=json.load(open(sys.argv[1],encoding="utf-8"))
if runtime.get("browser_running"):
    raise SystemExit(f"QWEN ACCEPTANCE FAILED: Chromium running during {sys.argv[2]}")
PY
}

assert_direct_execution() {
  local phase="$1" expected_external="$2" output="$TMP_DIR/runtime-exec-$RANDOM.json"
  api GET "/_llmgateway/browser-accounts/$ACCOUNT_ID/runtime" "$output"
  python3 - "$output" "$phase" "$expected_external" <<'PY'
import json,sys
runtime=json.load(open(sys.argv[1],encoding="utf-8"))
phase,model=sys.argv[2:4]
last=runtime.get("last_execution") or {}
if runtime.get("browser_running"):
    raise SystemExit(f"QWEN ACCEPTANCE FAILED: Chromium running after {phase}")
for key,value in {"transport":"direct-http","adapter_id":"qwen-web-http","browser_fallback":False}.items():
    if last.get(key) != value:
        raise SystemExit(f"QWEN ACCEPTANCE FAILED: {phase} telemetry mismatch for {key}: {last}")
if last.get("model") != model:
    raise SystemExit(f"QWEN ACCEPTANCE FAILED: {phase} executed {last.get('model')!r}, expected {model!r}")
PY
}

create_thread() {
  local title="$1" model="$2" output="$3" body
  body="$(python3 - "$title" "$model" <<'PY'
import json,sys
print(json.dumps({"title":sys.argv[1],"model":sys.argv[2]}))
PY
)"
  api POST "/v1/threads" "$output" "$body"
  LAST_THREAD="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["id"])' "$output")"
  THREADS+=("$LAST_THREAD")
}

send_turn() {
  local thread="$1" model="$2" external="$3" prompt="$4" prefix="$5" body
  body="$(python3 - "$prompt" "$model" <<'PY'
import json,sys
print(json.dumps({"content":sys.argv[1],"model":sys.argv[2],"stream":False}))
PY
)"
  curl -fsS -D "$TMP_DIR/$prefix.headers" -o "$TMP_DIR/$prefix.json" \
    -X POST -H "Authorization: Bearer $API_KEY" -H "Content-Type: application/json" \
    --data-binary "$body" "$BASE_URL/v1/threads/$thread/messages"
  local actual expected
  actual="$(awk 'BEGIN{IGNORECASE=1} /^x-llmgateway-route:/{gsub("\r",""); sub(/^[^:]+:[[:space:]]*/,""); print; exit}' "$TMP_DIR/$prefix.headers")"
  expected="discovered:$ACCOUNT_ID:$external"
  [[ "$actual" == "$expected" ]] || { echo "QWEN ACCEPTANCE FAILED: route '$actual' != '$expected'" >&2; exit 1; }
  python3 - "$TMP_DIR/$prefix.json" <<'PY'
import json,sys
body=json.load(open(sys.argv[1],encoding="utf-8"))
text=((body.get("choices") or [{}])[0].get("message") or {}).get("content")
if not str(text or "").strip():
    raise SystemExit("QWEN ACCEPTANCE FAILED: buffered turn returned empty assistant output")
PY
  assert_direct_execution "$prefix" "$external"
}

send_stream() {
  local thread="$1" model="$2" external="$3" prompt="$4" prefix="$5" body
  body="$(python3 - "$prompt" "$model" <<'PY'
import json,sys
print(json.dumps({"content":sys.argv[1],"model":sys.argv[2],"stream":True}))
PY
)"
  curl -fsS --no-buffer -D "$TMP_DIR/$prefix.headers" -o "$TMP_DIR/$prefix.sse" \
    -X POST -H "Authorization: Bearer $API_KEY" -H "Content-Type: application/json" \
    --data-binary "$body" "$BASE_URL/v1/threads/$thread/messages"
  grep -Eq '^data:[[:space:]]*\[DONE\][[:space:]]*$' "$TMP_DIR/$prefix.sse" \
    || { echo "QWEN ACCEPTANCE FAILED: stream ended without [DONE]" >&2; exit 1; }
  grep -Fq '"content"' "$TMP_DIR/$prefix.sse" \
    || { echo "QWEN ACCEPTANCE FAILED: stream returned no assistant delta" >&2; exit 1; }
  grep -Eq '"finish_reason"[[:space:]]*:[[:space:]]*"[^"]+"' "$TMP_DIR/$prefix.sse" \
    || { echo "QWEN ACCEPTANCE FAILED: stream returned no terminal finish_reason" >&2; exit 1; }
  assert_direct_execution "$prefix" "$external"
}

affinity() {
  local thread="$1" output="$2"
  api GET "/_llmgateway/threads/$thread/browser-affinity/$ACCOUNT_ID" "$output"
}

qwen_state() {
  python3 - "$1" <<'PY'
import json,sys
x=json.load(open(sys.argv[1],encoding="utf-8"))
print(json.dumps(((x.get("state") or {}).get("native_chain") or {}),sort_keys=True,separators=(",",":")))
PY
}

route_health() {
  local model="$1" output="$2" body
  body="$(python3 - "$model" <<'PY'
import json,sys
print(json.dumps({"model":sys.argv[1]}))
PY
)"
  api POST "/_llmgateway/routes/explain" "$output" "$body"
  python3 - "$output" "$ACCOUNT_ID" >"$output.health" <<'PY'
import json,sys
x=json.load(open(sys.argv[1],encoding="utf-8"))
matches=[c for c in x.get("candidates",[]) if c.get("account")==sys.argv[2]]
if not matches:
    raise SystemExit("QWEN ACCEPTANCE FAILED: route explain has no account candidate")
print(json.dumps(matches[0].get("route_health") or {},sort_keys=True,separators=(",",":")))
PY
}

step "Stopping Chromium and checking direct readiness"
api GET "/_llmgateway/browser-accounts/$ACCOUNT_ID/runtime" "$TMP_DIR/preflight.json"
SESSION_ID="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1])).get("session_id",""))' "$TMP_DIR/preflight.json")"
BROWSER_RUNNING="$(python3 -c 'import json,sys; print("1" if json.load(open(sys.argv[1])).get("browser_running") else "0")' "$TMP_DIR/preflight.json")"
if [[ "$BROWSER_RUNNING" == "1" ]]; then
  api POST "/_llmgateway/browser-sessions/$SESSION_ID/driver/stop" "$TMP_DIR/stop.json"
  sleep 0.3
fi
api GET "/_llmgateway/browser-accounts/$ACCOUNT_ID/runtime" "$TMP_DIR/preflight-after.json"
python3 - "$TMP_DIR/preflight-after.json" <<'PY'
import json,sys
runtime=json.load(open(sys.argv[1],encoding="utf-8"))
if runtime.get("browser_running"):
    raise SystemExit("QWEN ACCEPTANCE FAILED: Chromium could not be stopped")
if not runtime.get("auth_snapshot_available"):
    raise SystemExit("QWEN ACCEPTANCE FAILED: BrowserAuthMaterial snapshot is unavailable")
if not runtime.get("direct_ready") or runtime.get("effective_transport") != "direct-http":
    raise SystemExit(f"QWEN ACCEPTANCE FAILED: account is not http-preferred/direct-ready: {runtime}")
PY

step "Refreshing Qwen model catalog with Chromium stopped"
api POST "/_llmgateway/accounts/$ACCOUNT_ID/models/refresh" "$TMP_DIR/refresh.json"
api GET "/_llmgateway/accounts/$ACCOUNT_ID/models" "$TMP_DIR/models.json"
python3 - "$TMP_DIR/refresh.json" "$TMP_DIR/models.json" "$ACCOUNT_ID" "$MODEL_A" "$MODEL_B" "$TMP_DIR/selected.json" <<'PY'
import json,sys
refresh_path,models_path,account,requested_a,requested_b,out=sys.argv[1:]
refresh=json.load(open(refresh_path,encoding="utf-8"))
if int(refresh.get("discovered_models",0)) < 2:
    raise SystemExit(f"QWEN ACCEPTANCE FAILED: discovery returned fewer than two models: {refresh}")
payload=json.load(open(models_path,encoding="utf-8"))
models=[]
for model in payload.get("data",[]):
    bindings=model.get("accounts") or []
    if any(b.get("account_id")==account and b.get("enabled") and b.get("availability")=="available" and b.get("discovered") for b in bindings):
        models.append(model)
if len(models) < 2:
    raise SystemExit("QWEN ACCEPTANCE FAILED: account catalog has fewer than two discovered models")
def pick(requested,index):
    if requested:
        for model in models:
            if model.get("id")==requested or model.get("external_id")==requested:
                return model
        raise SystemExit(f"QWEN ACCEPTANCE FAILED: requested model {requested!r} was not discovered")
    return models[index]
a,b=pick(requested_a,0),pick(requested_b,1)
if a.get("id")==b.get("id"):
    raise SystemExit("QWEN ACCEPTANCE FAILED: model A and model B are identical")
json.dump({"a":a,"b":b},open(out,"w",encoding="utf-8"))
print(f"[qwen-browserless-live] Model A: {a.get('display_name')} [{a.get('id')}]")
print(f"[qwen-browserless-live] Model B: {b.get('display_name')} [{b.get('id')}]")
PY
MODEL_A_ID="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["a"]["id"])' "$TMP_DIR/selected.json")"
MODEL_A_EXTERNAL="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["a"]["external_id"])' "$TMP_DIR/selected.json")"
MODEL_B_ID="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["b"]["id"])' "$TMP_DIR/selected.json")"
MODEL_B_EXTERNAL="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["b"]["external_id"])' "$TMP_DIR/selected.json")"

api GET "/v1/models" "$TMP_DIR/public-models.json"
python3 - "$TMP_DIR/public-models.json" "$MODEL_A_ID" "$MODEL_B_ID" <<'PY'
import json,sys
ids={str(x.get("id") or "") for x in json.load(open(sys.argv[1],encoding="utf-8")).get("data",[])}
for model in sys.argv[2:]:
    if model not in ids:
        raise SystemExit(f"QWEN ACCEPTANCE FAILED: /v1/models does not expose {model!r}")
PY
api GET "/_llmgateway/browser-accounts/$ACCOUNT_ID/runtime" "$TMP_DIR/catalog-runtime.json"
python3 - "$TMP_DIR/catalog-runtime.json" <<'PY'
import json,sys
runtime=json.load(open(sys.argv[1],encoding="utf-8"))
catalog=runtime.get("model_catalog") or {}
if int(catalog.get("count") or 0) < 2 or catalog.get("refresh_required") or not catalog.get("discovered_at"):
    raise SystemExit(f"QWEN ACCEPTANCE FAILED: runtime model catalog incomplete: {catalog}")
if runtime.get("browser_running"):
    raise SystemExit("QWEN ACCEPTANCE FAILED: model refresh launched Chromium")
PY

step "Thread A turn 1"
create_thread "Qwen browserless A" "$MODEL_A_ID" "$TMP_DIR/thread-a.json"
THREAD_A="$LAST_THREAD"
send_turn "$THREAD_A" "$MODEL_A_ID" "$MODEL_A_EXTERNAL" "Reply briefly with: qwen-a1" "a1"
affinity "$THREAD_A" "$TMP_DIR/affinity-a1.json"
python3 - "$TMP_DIR/affinity-a1.json" <<'PY'
import json,sys
x=json.load(open(sys.argv[1],encoding="utf-8"))
q=((x.get("state") or {}).get("native_chain") or {})
if (x.get("state") or {}).get("transport") != "qwen-http":
    raise SystemExit(f"QWEN ACCEPTANCE FAILED: native state transport mismatch: {x.get('state')}")
if not q.get("conversation_id") or not q.get("response_id"):
    raise SystemExit(f"QWEN ACCEPTANCE FAILED: A1 missing chat/response ids: {q}")
if q.get("request_parent_id") not in (None,""):
    raise SystemExit(f"QWEN ACCEPTANCE FAILED: first turn unexpectedly had request parent: {q}")
PY
CHAT_A="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["state"]["native_chain"]["conversation_id"])' "$TMP_DIR/affinity-a1.json")"
PARENT_A1="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["state"]["native_chain"].get("parent_id") or "")' "$TMP_DIR/affinity-a1.json")"
RESPONSE_A1="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["state"]["native_chain"]["response_id"])' "$TMP_DIR/affinity-a1.json")"

step "Thread A turn 2 reuses chat_id and chains response_id"
send_turn "$THREAD_A" "$MODEL_A_ID" "$MODEL_A_EXTERNAL" "Reply briefly with: qwen-a2" "a2"
affinity "$THREAD_A" "$TMP_DIR/affinity-a2.json"
python3 - "$TMP_DIR/affinity-a2.json" "$CHAT_A" "$RESPONSE_A1" "$PARENT_A1" <<'PY'
import json,sys
x=json.load(open(sys.argv[1],encoding="utf-8"))
q=((x.get("state") or {}).get("native_chain") or {})
chat,response1,parent1=sys.argv[2:5]
if q.get("conversation_id") != chat:
    raise SystemExit(f"QWEN ACCEPTANCE FAILED: same thread changed chat_id: {q}")
if q.get("request_parent_id") != response1:
    raise SystemExit(f"QWEN ACCEPTANCE FAILED: A2 did not chain previous response_id: {q}")
if not q.get("response_id") or q.get("response_id") == response1:
    raise SystemExit(f"QWEN ACCEPTANCE FAILED: response_id did not advance: {q}")
if parent1 and q.get("parent_id") == parent1:
    raise SystemExit(f"QWEN ACCEPTANCE FAILED: upstream parent_id did not advance: {q}")
PY
RESPONSE_A2="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["state"]["native_chain"]["response_id"])' "$TMP_DIR/affinity-a2.json")"

step "Thread B on model B is independent"
create_thread "Qwen browserless B" "$MODEL_B_ID" "$TMP_DIR/thread-b.json"
THREAD_B="$LAST_THREAD"
send_turn "$THREAD_B" "$MODEL_B_ID" "$MODEL_B_EXTERNAL" "Reply briefly with: qwen-b1" "b1"
affinity "$THREAD_B" "$TMP_DIR/affinity-b1.json"
python3 - "$TMP_DIR/affinity-b1.json" "$CHAT_A" <<'PY'
import json,sys
q=((json.load(open(sys.argv[1],encoding="utf-8")).get("state") or {}).get("native_chain") or {})
if not q.get("conversation_id") or q.get("conversation_id") == sys.argv[2]:
    raise SystemExit(f"QWEN ACCEPTANCE FAILED: Thread B is not independent: {q}")
if not q.get("response_id"):
    raise SystemExit(f"QWEN ACCEPTANCE FAILED: Thread B missing response_id: {q}")
PY

step "Rejecting a mid-thread model switch without route-health mutation"
route_health "$MODEL_B_ID" "$TMP_DIR/route-before.json"
STATE_BEFORE="$(qwen_state "$TMP_DIR/affinity-a2.json")"
api GET "/v1/threads/$THREAD_A" "$TMP_DIR/thread-before-switch.json"
COUNT_BEFORE="$(python3 -c 'import json,sys; print(len(json.load(open(sys.argv[1])).get("messages",[])))' "$TMP_DIR/thread-before-switch.json")"
SWITCH_BODY="$(python3 - "$MODEL_B_ID" <<'PY'
import json,sys
print(json.dumps({"content":"This must be rejected before provider submit.","model":sys.argv[1],"stream":False}))
PY
)"
set +e
SWITCH_CODE="$(curl -sS -o "$TMP_DIR/model-switch.json" -w '%{http_code}' \
  -H "Authorization: Bearer $API_KEY" -H "Content-Type: application/json" \
  --data-binary "$SWITCH_BODY" "$BASE_URL/v1/threads/$THREAD_A/messages")"
set -e
[[ "$SWITCH_CODE" -ge 400 ]] || { echo "QWEN ACCEPTANCE FAILED: model switch unexpectedly succeeded" >&2; exit 1; }
grep -Fq 'start a new llmgateway thread' "$TMP_DIR/model-switch.json" || {
  echo "QWEN ACCEPTANCE FAILED: model switch did not use native affinity rejection" >&2
  cat "$TMP_DIR/model-switch.json" >&2
  exit 1
}
assert_browser_stopped "model switch rejection"
affinity "$THREAD_A" "$TMP_DIR/affinity-after-switch.json"
STATE_AFTER="$(qwen_state "$TMP_DIR/affinity-after-switch.json")"
[[ "$STATE_AFTER" == "$STATE_BEFORE" ]] || { echo "QWEN ACCEPTANCE FAILED: rejected switch mutated native state" >&2; exit 1; }
api GET "/v1/threads/$THREAD_A" "$TMP_DIR/thread-after-switch.json"
COUNT_AFTER="$(python3 -c 'import json,sys; print(len(json.load(open(sys.argv[1])).get("messages",[])))' "$TMP_DIR/thread-after-switch.json")"
[[ "$COUNT_AFTER" == "$COUNT_BEFORE" ]] || { echo "QWEN ACCEPTANCE FAILED: rejected switch persisted a local message" >&2; exit 1; }
route_health "$MODEL_B_ID" "$TMP_DIR/route-after.json"
cmp -s "$TMP_DIR/route-before.json.health" "$TMP_DIR/route-after.json.health" || {
  echo "QWEN ACCEPTANCE FAILED: rejected switch mutated route health/cooldown" >&2
  diff -u "$TMP_DIR/route-before.json.health" "$TMP_DIR/route-after.json.health" >&2 || true
  exit 1
}

step "Streaming must logically complete before native/local persistence"
send_stream "$THREAD_A" "$MODEL_A_ID" "$MODEL_A_EXTERNAL" "Reply briefly with: qwen-a-stream" "a-stream"
sleep 0.3
affinity "$THREAD_A" "$TMP_DIR/affinity-a3.json"
python3 - "$TMP_DIR/affinity-a3.json" "$CHAT_A" "$RESPONSE_A2" <<'PY'
import json,sys
q=((json.load(open(sys.argv[1],encoding="utf-8")).get("state") or {}).get("native_chain") or {})
if q.get("conversation_id") != sys.argv[2]:
    raise SystemExit(f"QWEN ACCEPTANCE FAILED: streaming changed chat_id: {q}")
if q.get("request_parent_id") != sys.argv[3]:
    raise SystemExit(f"QWEN ACCEPTANCE FAILED: streaming did not chain prior response_id: {q}")
if not q.get("response_id") or q.get("response_id") == sys.argv[3]:
    raise SystemExit(f"QWEN ACCEPTANCE FAILED: streaming response_id did not advance: {q}")
PY
api GET "/v1/threads/$THREAD_A" "$TMP_DIR/thread-after-stream.json"
python3 - "$TMP_DIR/thread-after-stream.json" <<'PY'
import json,sys
thread=json.load(open(sys.argv[1],encoding="utf-8"))
for message in thread.get("messages",[]):
    if message.get("role")=="assistant":
        content=message.get("content") or (message.get("message") or {}).get("content")
        if not str(content or "").strip():
            raise SystemExit("QWEN ACCEPTANCE FAILED: stream persisted an empty/stale assistant")
PY

if [[ "$TEST_CANCELLATION" -eq 1 ]]; then
  step "Cancellation must not persist an assistant"
  create_thread "Qwen browserless cancellation" "$MODEL_A_ID" "$TMP_DIR/thread-c.json"
  THREAD_C="$LAST_THREAD"
  CANCEL_BODY="$(python3 - "$MODEL_A_ID" <<'PY'
import json,sys
print(json.dumps({"content":"Write at least 1200 words and begin with cancellation-qwen.","model":sys.argv[1],"stream":True}))
PY
)"
  set +e
  curl -sS --no-buffer --max-time 1 \
    -H "Authorization: Bearer $API_KEY" -H "Content-Type: application/json" \
    --data-binary "$CANCEL_BODY" "$BASE_URL/v1/threads/$THREAD_C/messages" >/dev/null
  CURL_EXIT=$?
  set -e
  if [[ "$CURL_EXIT" -eq 28 ]]; then
    sleep 2
    assert_browser_stopped "cancelled stream"
    api GET "/v1/threads/$THREAD_C" "$TMP_DIR/thread-cancel.json"
    python3 - "$TMP_DIR/thread-cancel.json" <<'PY'
import json,sys
thread=json.load(open(sys.argv[1],encoding="utf-8"))
if any(m.get("role")=="assistant" for m in thread.get("messages",[])):
    raise SystemExit("QWEN ACCEPTANCE FAILED: cancelled stream persisted an assistant")
PY
  else
    echo "[qwen-browserless-live] WARNING: cancellation stream finished before timeout; cancellation cleanup was not exercised."
  fi
fi

assert_browser_stopped "final gate"
echo
echo "QWEN BROWSERLESS LIVE ACCEPTANCE: PASS"
echo "Account: $ACCOUNT_ID"
echo "Model A: $MODEL_A_ID -> discovered:$ACCOUNT_ID:$MODEL_A_EXTERNAL"
echo "Model B: $MODEL_B_ID -> discovered:$ACCOUNT_ID:$MODEL_B_EXTERNAL"
echo "Thread A chat_id: $CHAT_A"
echo "Chromium running: false"
