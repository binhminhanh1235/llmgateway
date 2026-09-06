#!/usr/bin/env bash
set -euo pipefail

BASE_URL="${LLMGATEWAY_BASE_URL:-http://127.0.0.1:7331}"
API_KEY="${LLMGATEWAY_API_KEY:-}"
ACCOUNT_ID=""
MODEL_ID=""
IMAGE_PATH=""
KEEP_THREAD=0
KEEP_FILE=0
AUTO_LAUNCH=1
READY_TIMEOUT_SECONDS="${LLMGATEWAY_VISION_READY_TIMEOUT_SECONDS:-120}"
RUNTIME_BASELINE=""

usage() {
  cat <<'EOF'
Usage:
  live-vision-acceptance.sh --account <id> [options]

Options:
  --base-url <url>    Gateway URL (default: http://127.0.0.1:7331)
  --api-key <key>     Gateway API key (or set LLMGATEWAY_API_KEY)
  --model <id>        Force a selectable model/route; otherwise resolve a vision route for the account
  --image <path>      PNG/JPEG/WebP/GIF to upload; otherwise generate a valid 1x1 PNG
  --keep-thread       Keep the generated acceptance thread
  --keep-file         Keep the uploaded artifact after acceptance
  --no-auto-launch    Do not open the isolated Chromium profile when CDP is unavailable
  --ready-timeout <s> Seconds to wait for an authenticated/ready browser session (default: 120)
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --account) ACCOUNT_ID="${2:-}"; shift 2 ;;
    --base-url) BASE_URL="${2:-}"; shift 2 ;;
    --api-key) API_KEY="${2:-}"; shift 2 ;;
    --model) MODEL_ID="${2:-}"; shift 2 ;;
    --image) IMAGE_PATH="${2:-}"; shift 2 ;;
    --keep-thread) KEEP_THREAD=1; shift ;;
    --keep-file) KEEP_FILE=1; shift ;;
    --no-auto-launch) AUTO_LAUNCH=0; shift ;;
    --ready-timeout) READY_TIMEOUT_SECONDS="${2:-}"; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "Unknown argument: $1" >&2; usage >&2; exit 2 ;;
  esac
done

[[ -n "$ACCOUNT_ID" ]] || { echo "--account is required" >&2; exit 2; }
[[ -n "$API_KEY" ]] || { echo "--api-key is required or set LLMGATEWAY_API_KEY" >&2; exit 2; }
command -v curl >/dev/null || { echo "curl is required" >&2; exit 2; }
command -v python3 >/dev/null || { echo "python3 is required" >&2; exit 2; }
[[ "$READY_TIMEOUT_SECONDS" =~ ^[1-9][0-9]*$ ]] || {
  echo "--ready-timeout must be a positive integer" >&2
  exit 2
}

BASE_URL="${BASE_URL%/}"
TMP_DIR="$(mktemp -d)"
THREAD_ID=""
FILE_ID=""
GENERATED_IMAGE=0

cleanup() {
  if [[ "$KEEP_THREAD" -eq 0 && -n "$THREAD_ID" ]]; then
    curl -fsS -X DELETE -H "Authorization: Bearer $API_KEY"       "$BASE_URL/v1/threads/$THREAD_ID" >/dev/null 2>&1 || true
    THREAD_ID=""
  fi
  if [[ "$KEEP_FILE" -eq 0 && -n "$FILE_ID" ]]; then
    curl -fsS -X DELETE -H "Authorization: Bearer $API_KEY"       "$BASE_URL/v1/files/$FILE_ID" >/dev/null 2>&1 || true
    FILE_ID=""
  fi
  rm -rf "$TMP_DIR"
}
trap cleanup EXIT

step() { echo "[vision-live] $*"; }

api_json() {
  local method="$1" path="$2" output="$3" body="${4:-}"
  local args=(-fsS -X "$method" -H "Authorization: Bearer $API_KEY")
  if [[ -n "$body" ]]; then
    args+=(-H "Content-Type: application/json" --data-binary "$body")
  fi
  curl "${args[@]}" "$BASE_URL$path" -o "$output"
}

json_assert() {
  local file="$1" expr="$2" message="$3"
  python3 - "$file" "$expr" "$message" <<'PY'
import json,sys
path,expr,message=sys.argv[1:4]
with open(path,encoding="utf-8") as f:
    x=json.load(f)
safe={"bool":bool,"int":int,"str":str,"len":len,"any":any,"all":all}
if not eval(expr,{"__builtins__":safe},{"x":x}):
    raise SystemExit("VISION ACCEPTANCE FAILED: "+message+"\n"+json.dumps(x,indent=2))
PY
}

header_value() {
  local file="$1" name="$2"
  python3 - "$file" "$name" <<'PY'
import sys
path,name=sys.argv[1:3]
needle=name.lower()+":"
for raw in open(path,encoding="utf-8",errors="replace"):
    line=raw.strip()
    if line.lower().startswith(needle):
        print(line.split(":",1)[1].strip())
        break
PY
}

runtime() {
  api_json GET "/_llmgateway/browser-accounts/$ACCOUNT_ID/runtime" "$1"
}

driver_status() {
  api_json GET "/_llmgateway/browser-sessions/$SESSION_ID/driver/status" "$1"
}

driver_launch() {
  api_json POST "/_llmgateway/browser-sessions/$SESSION_ID/driver/launch" "$1"
}

driver_verify() {
  api_json POST "/_llmgateway/browser-sessions/$SESSION_ID/driver/verify" "$1"
}

json_value() {
  local file="$1" expr="$2"
  python3 - "$file" "$expr" <<'PY'
import json,sys
path,expr=sys.argv[1:3]
with open(path,encoding="utf-8") as f:
    x=json.load(f)
safe={"bool":bool,"int":int,"str":str,"len":len,"any":any,"all":all}
value=eval(expr,{"__builtins__":safe},{"x":x})
if isinstance(value,bool):
    print("true" if value else "false")
elif value is None:
    print("")
else:
    print(value)
PY
}

ensure_browser_ready() {
  local initial_runtime="$1"
  local current_runtime="$TMP_DIR/runtime-ready.json"
  local status_file="$TMP_DIR/driver-status.json"
  local verify_file="$TMP_DIR/driver-verify.json"
  local browser_running direct_ready

  cp "$initial_runtime" "$current_runtime"
  browser_running="$(json_value "$current_runtime" "bool(x.get('browser_running'))")"
  direct_ready="$(json_value "$current_runtime" "bool(x.get('direct_ready'))")"

  # A direct-ready account with Chromium closed is intentional. Image execution
  # should open a temporary CDP fallback and restore the browser-closed posture.
  if [[ "$direct_ready" == "true" && "$browser_running" != "true" ]]; then
    step "Direct auth is ready with Chromium closed; vision will exercise temporary CDP fallback"
    RUNTIME_BASELINE="$current_runtime"
    return
  fi

  if [[ "$browser_running" != "true" ]]; then
    if [[ "$AUTO_LAUNCH" -ne 1 ]]; then
      echo "VISION ACCEPTANCE FAILED: Chromium is not running and --no-auto-launch was requested" >&2
      exit 1
    fi
    step "Opening isolated Chromium session '$SESSION_ID'"
    driver_launch "$TMP_DIR/driver-launch.json"
  else
    step "Chromium is already running; checking authenticated provider page"
  fi

  local max_polls=$((READY_TIMEOUT_SECONDS * 2))
  local poll=0
  while (( poll < max_polls )); do
    poll=$((poll + 1))
    driver_status "$status_file"
    local running ready_match
    running="$(json_value "$status_file" "bool(x.get('running'))")"
    ready_match="$(json_value "$status_file" "str(x.get('ready_match') or '')")"

    if [[ "$running" == "true" && -n "$ready_match" ]]; then
      driver_verify "$verify_file"
      sleep 0.25
    fi

    runtime "$current_runtime"
    if [[ "$(json_value "$current_runtime" "bool(x.get('direct_ready')) or (bool(x.get('browser_running')) and bool(x.get('session',{}).get('routable')) and x.get('adapter',{}).get('status') == 'ready')")" == "true" ]]; then
      RUNTIME_BASELINE="$current_runtime"
      local effective
      effective="$(json_value "$current_runtime" "str(x.get('effective_transport') or '')")"
      step "Browser account is ready for vision execution (effective transport: $effective)"
      return
    fi

    sleep 0.5
  done

  runtime "$current_runtime"
  echo "VISION ACCEPTANCE FAILED: browser account did not become authenticated/ready within ${READY_TIMEOUT_SECONDS}s" >&2
  echo "Complete the normal provider login in the opened isolated Chromium window, then rerun the acceptance." >&2
  python3 -m json.tool "$current_runtime" >&2 || cat "$current_runtime" >&2
  exit 1
}

if [[ -z "$IMAGE_PATH" ]]; then
  IMAGE_PATH="$TMP_DIR/vision-1x1.png"
  python3 - "$IMAGE_PATH" <<'PY'
import base64,sys
payload="iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAusB9Wl8v9sAAAAASUVORK5CYII="
with open(sys.argv[1],"wb") as f:
    f.write(base64.b64decode(payload))
PY
  GENERATED_IMAGE=1
fi
[[ -f "$IMAGE_PATH" ]] || { echo "Image does not exist: $IMAGE_PATH" >&2; exit 2; }

step "Checking gateway and browser account runtime"
api_json GET "/_llmgateway/health" "$TMP_DIR/health.json"
json_assert "$TMP_DIR/health.json" "x.get('status') == 'ok'" "gateway health is not ok"
runtime "$TMP_DIR/runtime-before.json"
json_assert "$TMP_DIR/runtime-before.json" "x.get('provider_kind') in ['browser-chatgpt','browser-gemini']" "P2 live gate currently supports verified ChatGPT/Gemini browser adapters only"
json_assert "$TMP_DIR/runtime-before.json" "bool(x.get('session',{}).get('enabled'))" "browser session is disabled"

PROVIDER_KIND="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["provider_kind"])' "$TMP_DIR/runtime-before.json")"
SESSION_ID="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1])).get("session_id",""))' "$TMP_DIR/runtime-before.json")"
[[ -n "$SESSION_ID" ]] || { echo "VISION ACCEPTANCE FAILED: browser account has no session id" >&2; exit 1; }

ensure_browser_ready "$TMP_DIR/runtime-before.json"
[[ -n "$RUNTIME_BASELINE" ]] || { echo "VISION ACCEPTANCE FAILED: runtime readiness baseline was not established" >&2; exit 1; }

step "Resolving a vision-capable selectable model for account '$ACCOUNT_ID'"
api_json GET "/v1/models" "$TMP_DIR/models.json"
if [[ -z "$MODEL_ID" ]]; then
  MODEL_ID="$(python3 - "$TMP_DIR/models.json" "$ACCOUNT_ID" <<'PY'
import json,sys
payload=json.load(open(sys.argv[1],encoding="utf-8"))
account=sys.argv[2]
for model in payload.get("data",[]):
    meta=model.get("llmgateway") or {}
    caps=meta.get("multimodal_capabilities") or {}
    if meta.get("kind")=="route" and meta.get("account")==account and "image" in (caps.get("input_modalities") or []):
        print(model.get("id",""))
        break
PY
)"
fi
[[ -n "$MODEL_ID" ]] || {
  echo "VISION ACCEPTANCE FAILED: no vision-capable route is selectable for '$ACCOUNT_ID'" >&2
  exit 1
}

python3 - "$TMP_DIR/models.json" "$MODEL_ID" <<'PY'
import json,sys
payload=json.load(open(sys.argv[1],encoding="utf-8"))
model_id=sys.argv[2]
model=next((m for m in payload.get("data",[]) if m.get("id")==model_id),None)
if model is None:
    raise SystemExit(f"VISION ACCEPTANCE FAILED: selected model {model_id!r} is not exposed by /v1/models")
caps=(model.get("llmgateway") or {}).get("multimodal_capabilities") or {}
if "image" not in (caps.get("input_modalities") or []):
    raise SystemExit(f"VISION ACCEPTANCE FAILED: selected model {model_id!r} does not advertise image input: {caps}")
PY
step "Using model '$MODEL_ID'"

step "Uploading image artifact"
FILE_JSON="$TMP_DIR/file.json"
curl -fsS -X POST "$BASE_URL/v1/files"   -H "Authorization: Bearer $API_KEY"   -F 'purpose=vision'   -F "file=@$IMAGE_PATH"   -o "$FILE_JSON"
FILE_ID="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1])).get("id",""))' "$FILE_JSON")"
[[ -n "$FILE_ID" ]] || { echo "VISION ACCEPTANCE FAILED: /v1/files returned no id" >&2; exit 1; }
step "Stored image as '$FILE_ID'"

python3 - "$MODEL_ID" "$FILE_ID" "$TMP_DIR/chat.json" "$TMP_DIR/responses.json" <<'PY'
import json,sys
model,file_id,chat_path,responses_path=sys.argv[1:5]
chat={
  "model":model,
  "stream":False,
  "messages":[{
    "role":"user",
    "content":[
      {"type":"text","text":"Describe the attached image briefly. Mention that you received an image."},
      {"type":"image_url","file_id":file_id}
    ]
  }]
}
responses={
  "model":model,
  "stream":False,
  "input":[{
    "type":"message",
    "role":"user",
    "content":[
      {"type":"input_text","text":"Look at this same stored image again and answer in one short sentence."},
      {"type":"input_image","file_id":file_id}
    ]
  }]
}
json.dump(chat,open(chat_path,"w",encoding="utf-8"))
json.dump(responses,open(responses_path,"w",encoding="utf-8"))
PY

step "Scenario 1/3: Chat Completions image + text"
curl -fsS -D "$TMP_DIR/chat.headers" -o "$TMP_DIR/chat.out.json"   -X POST "$BASE_URL/v1/chat/completions"   -H "Authorization: Bearer $API_KEY"   -H "Content-Type: application/json"   --data-binary "@$TMP_DIR/chat.json"
json_assert "$TMP_DIR/chat.out.json" "bool((((x.get('choices') or [{}])[0].get('message') or {}).get('content')))" "Chat Completions returned empty assistant text"
ROUTE_1="$(header_value "$TMP_DIR/chat.headers" "x-llmgateway-route")"
[[ -n "$ROUTE_1" ]] || { echo "VISION ACCEPTANCE FAILED: Chat Completions returned no route header" >&2; exit 1; }

sleep 0.4
runtime "$TMP_DIR/runtime-chat.json"
python3 - "$TMP_DIR/runtime-chat.json" "$PROVIDER_KIND" "$ROUTE_1" <<'PY'
import json,sys
runtime=json.load(open(sys.argv[1],encoding="utf-8"))
provider,route=sys.argv[2:4]
last=runtime.get("last_execution") or {}
if last.get("transport") != "browser-cdp":
    raise SystemExit(f"VISION ACCEPTANCE FAILED: image turn used {last.get('transport')!r}, expected browser-cdp: {last}")
expected_adapter={"browser-chatgpt":"chatgpt-web","browser-gemini":"gemini-web"}[provider]
if last.get("adapter_id") != expected_adapter:
    raise SystemExit(f"VISION ACCEPTANCE FAILED: image turn used adapter {last.get('adapter_id')!r}, expected {expected_adapter!r}: {last}")
PY

step "Scenario 2/3: Responses reuses the same stored image"
curl -fsS -D "$TMP_DIR/responses.headers" -o "$TMP_DIR/responses.out.json"   -X POST "$BASE_URL/v1/responses"   -H "Authorization: Bearer $API_KEY"   -H "Content-Type: application/json"   --data-binary "@$TMP_DIR/responses.json"
python3 - "$TMP_DIR/responses.out.json" <<'PY'
import json,sys
x=json.load(open(sys.argv[1],encoding="utf-8"))
texts=[]
for item in x.get("output") or []:
    for part in item.get("content") or []:
        if part.get("type")=="output_text" and str(part.get("text") or "").strip():
            texts.append(part["text"])
if not texts:
    raise SystemExit("VISION ACCEPTANCE FAILED: Responses returned no output_text\n"+json.dumps(x,indent=2))
PY
ROUTE_2="$(header_value "$TMP_DIR/responses.headers" "x-llmgateway-route")"
[[ -n "$ROUTE_2" ]] || { echo "VISION ACCEPTANCE FAILED: Responses returned no route header" >&2; exit 1; }

step "Scenario 3/3: Threads persists and reuses the image artifact reference"
THREAD_BODY="$(python3 - "$MODEL_ID" <<'PY'
import json,sys
print(json.dumps({"title":"P2 live vision acceptance","model":sys.argv[1]}))
PY
)"
api_json POST "/v1/threads" "$TMP_DIR/thread-create.json" "$THREAD_BODY"
THREAD_ID="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1])).get("id",""))' "$TMP_DIR/thread-create.json")"
[[ -n "$THREAD_ID" ]] || { echo "VISION ACCEPTANCE FAILED: thread creation returned no id" >&2; exit 1; }

THREAD_MESSAGE="$(python3 - "$MODEL_ID" "$FILE_ID" <<'PY'
import json,sys
print(json.dumps({
  "model":sys.argv[1],
  "stream":False,
  "content":[
    {"type":"input_text","text":"Use this image in the persisted thread and respond briefly."},
    {"type":"input_image","file_id":sys.argv[2]}
  ]
}))
PY
)"
api_json POST "/v1/threads/$THREAD_ID/messages" "$TMP_DIR/thread-reply.json" "$THREAD_MESSAGE"
json_assert "$TMP_DIR/thread-reply.json" "bool((((x.get('choices') or [{}])[0].get('message') or {}).get('content')))" "Threads vision turn returned empty assistant text"

api_json GET "/v1/threads/$THREAD_ID" "$TMP_DIR/thread-detail.json"
python3 - "$TMP_DIR/thread-detail.json" "$FILE_ID" <<'PY'
import json,sys
thread=json.load(open(sys.argv[1],encoding="utf-8"))
file_id=sys.argv[2]
uri="llmgateway://artifact/"+file_id
found=False
for row in thread.get("messages") or []:
    message=row.get("message") or {}
    content=message.get("content")
    if not isinstance(content,list):
        continue
    for part in content:
        if not isinstance(part,dict):
            continue
        image_url=part.get("image_url")
        url=image_url.get("url") if isinstance(image_url,dict) else image_url
        if url==uri:
            found=True
        if isinstance(url,str) and url.startswith("data:"):
            raise SystemExit("VISION ACCEPTANCE FAILED: thread persisted a data URL instead of an artifact reference")
if not found:
    raise SystemExit(f"VISION ACCEPTANCE FAILED: thread did not persist {uri}")
PY

step "Verifying reference-safe delete while thread owns the image"
STATUS="$(curl -sS -o "$TMP_DIR/delete-in-use.json" -w '%{http_code}' -X DELETE   "$BASE_URL/v1/files/$FILE_ID" -H "Authorization: Bearer $API_KEY")"
[[ "$STATUS" == "409" ]] || {
  echo "VISION ACCEPTANCE FAILED: deleting referenced image returned HTTP $STATUS instead of 409" >&2
  cat "$TMP_DIR/delete-in-use.json" >&2
  exit 1
}
json_assert "$TMP_DIR/delete-in-use.json" "x.get('error',{}).get('type') == 'artifact_in_use'" "referenced image did not return artifact_in_use"

if [[ "$KEEP_THREAD" -eq 0 ]]; then
  api_json DELETE "/v1/threads/$THREAD_ID" "$TMP_DIR/thread-delete.json"
  THREAD_ID=""
fi
if [[ "$KEEP_FILE" -eq 0 && "$KEEP_THREAD" -eq 0 ]]; then
  api_json DELETE "/v1/files/$FILE_ID" "$TMP_DIR/file-delete.json"
  FILE_ID=""
fi

step "Checking post-vision transport posture"
sleep 0.7
runtime "$TMP_DIR/runtime-after.json"
python3 - "$RUNTIME_BASELINE" "$TMP_DIR/runtime-after.json" <<'PY'
import json,sys
before=json.load(open(sys.argv[1],encoding="utf-8"))
after=json.load(open(sys.argv[2],encoding="utf-8"))
last=after.get("last_execution") or {}
if last.get("transport") != "browser-cdp":
    raise SystemExit(f"VISION ACCEPTANCE FAILED: final image execution telemetry is not browser-cdp: {last}")
# If Chromium was already closed and direct HTTP was ready before the image turn,
# vision should use a temporary CDP fallback and restore the browser-closed posture.
if not before.get("browser_running") and before.get("direct_ready"):
    if not last.get("browser_fallback"):
        raise SystemExit(f"VISION ACCEPTANCE FAILED: browserless-preferred image turn did not record browser fallback: {last}")
    if after.get("browser_running"):
        raise SystemExit("VISION ACCEPTANCE FAILED: temporary vision CDP browser was not released")
PY

step "PASS: live vision works through Chat Completions, Responses, Threads and ArtifactStore reuse"
step "provider=$PROVIDER_KIND account=$ACCOUNT_ID model=$MODEL_ID routes=$ROUTE_1,$ROUTE_2"
if [[ "$GENERATED_IMAGE" -eq 1 ]]; then
  step "acceptance used generated valid 1x1 PNG"
fi
