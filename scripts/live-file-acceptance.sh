#!/usr/bin/env bash
set -euo pipefail

BASE_URL="${LLMGATEWAY_BASE_URL:-http://127.0.0.1:7331}"
API_KEY="${LLMGATEWAY_API_KEY:-}"
ACCOUNT_ID=""
MODEL_ID=""
PDF_PATH=""
KEEP_THREAD=0
KEEP_FILE=0
AUTO_LAUNCH=1
READY_TIMEOUT_SECONDS="${LLMGATEWAY_FILE_READY_TIMEOUT_SECONDS:-120}"
RUNTIME_BASELINE=""

usage() {
  cat <<'EOF'
Usage:
  live-file-acceptance.sh --account <id> [options]

Options:
  --base-url <url>    Gateway URL (default: http://127.0.0.1:7331)
  --api-key <key>     Gateway API key (or set LLMGATEWAY_API_KEY)
  --model <id>        Force a selectable route; otherwise resolve a native-file route for the account
  --pdf <path>        PDF to upload; otherwise generate a small valid PDF fixture
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
    --pdf) PDF_PATH="${2:-}"; shift 2 ;;
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
GENERATED_PDF=0

cleanup() {
  if [[ "$KEEP_THREAD" -eq 0 && -n "$THREAD_ID" ]]; then
    curl -fsS -X DELETE -H "Authorization: Bearer $API_KEY" \
      "$BASE_URL/v1/threads/$THREAD_ID" >/dev/null 2>&1 || true
    THREAD_ID=""
  fi
  if [[ "$KEEP_FILE" -eq 0 && -n "$FILE_ID" ]]; then
    curl -fsS -X DELETE -H "Authorization: Bearer $API_KEY" \
      "$BASE_URL/v1/files/$FILE_ID" >/dev/null 2>&1 || true
    FILE_ID=""
  fi
  rm -rf "$TMP_DIR"
}
trap cleanup EXIT

step() { echo "[file-live] $*"; }

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
    raise SystemExit("FILE ACCEPTANCE FAILED: "+message+"\n"+json.dumps(x,indent=2))
PY
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

affinity() {
  api_json GET "/_llmgateway/threads/$THREAD_ID/browser-affinity/$ACCOUNT_ID" "$1"
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

ensure_browser_ready() {
  local initial_runtime="$1"
  local current_runtime="$TMP_DIR/runtime-ready.json"
  local status_file="$TMP_DIR/driver-status.json"
  local verify_file="$TMP_DIR/driver-verify.json"
  local browser_running direct_ready

  cp "$initial_runtime" "$current_runtime"
  browser_running="$(json_value "$current_runtime" "bool(x.get('browser_running'))")"
  direct_ready="$(json_value "$current_runtime" "bool(x.get('direct_ready'))")"

  # Native document upload is currently a CDP-only capability. A direct-ready
  # account with Chromium closed should use a temporary CDP fallback.
  if [[ "$direct_ready" == "true" && "$browser_running" != "true" ]]; then
    step "Direct auth is ready with Chromium closed; file upload will exercise temporary CDP fallback"
    RUNTIME_BASELINE="$current_runtime"
    return
  fi

  if [[ "$browser_running" != "true" ]]; then
    if [[ "$AUTO_LAUNCH" -ne 1 ]]; then
      echo "FILE ACCEPTANCE FAILED: Chromium is not running and --no-auto-launch was requested" >&2
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
      step "Browser account is ready for native file execution (effective transport: $effective)"
      return
    fi

    sleep 0.5
  done

  runtime "$current_runtime"
  echo "FILE ACCEPTANCE FAILED: browser account did not become authenticated/ready within ${READY_TIMEOUT_SECONDS}s" >&2
  echo "Complete the normal provider login in the opened isolated Chromium window, then rerun the acceptance." >&2
  python3 -m json.tool "$current_runtime" >&2 || cat "$current_runtime" >&2
  exit 1
}

if [[ -z "$PDF_PATH" ]]; then
  PDF_PATH="$TMP_DIR/p3-live.pdf"
  python3 - "$PDF_PATH" <<'PY'
import sys
path=sys.argv[1]
stream=b"BT /F1 12 Tf 36 100 Td (LLMGateway P3 native PDF acceptance) Tj ET"
objects=[
    b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n",
    b"2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n",
    b"3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 360 144] /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>\nendobj\n",
    b"4 0 obj\n<< /Length "+str(len(stream)).encode()+b" >>\nstream\n"+stream+b"\nendstream\nendobj\n",
    b"5 0 obj\n<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>\nendobj\n",
]
data=bytearray(b"%PDF-1.4\n")
offsets=[0]
for obj in objects:
    offsets.append(len(data))
    data.extend(obj)
xref=len(data)
data.extend(f"xref\n0 {len(objects)+1}\n".encode())
data.extend(b"0000000000 65535 f \n")
for offset in offsets[1:]:
    data.extend(f"{offset:010d} 00000 n \n".encode())
data.extend(
    f"trailer\n<< /Size {len(objects)+1} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n".encode()
)
with open(path,"wb") as f:
    f.write(data)
PY
  GENERATED_PDF=1
fi
[[ -f "$PDF_PATH" ]] || { echo "PDF does not exist: $PDF_PATH" >&2; exit 2; }
python3 - "$PDF_PATH" <<'PY'
import sys
raw=open(sys.argv[1],"rb").read(8)
if not raw.startswith(b"%PDF-"):
    raise SystemExit("FILE ACCEPTANCE FAILED: --pdf input is not a PDF")
PY

step "Checking gateway and browser account runtime"
api_json GET "/_llmgateway/health" "$TMP_DIR/health.json"
json_assert "$TMP_DIR/health.json" "x.get('status') == 'ok'" "gateway health is not ok"
runtime "$TMP_DIR/runtime-before.json"
json_assert "$TMP_DIR/runtime-before.json" "x.get('provider_kind') in ['browser-chatgpt','browser-gemini']" "P3 live gate currently supports verified ChatGPT/Gemini browser adapters only"
json_assert "$TMP_DIR/runtime-before.json" "bool(x.get('session',{}).get('enabled'))" "browser session is disabled"

PROVIDER_KIND="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["provider_kind"])' "$TMP_DIR/runtime-before.json")"
SESSION_ID="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1])).get("session_id",""))' "$TMP_DIR/runtime-before.json")"
[[ -n "$SESSION_ID" ]] || { echo "FILE ACCEPTANCE FAILED: browser account has no session id" >&2; exit 1; }

ensure_browser_ready "$TMP_DIR/runtime-before.json"
[[ -n "$RUNTIME_BASELINE" ]] || { echo "FILE ACCEPTANCE FAILED: runtime readiness baseline was not established" >&2; exit 1; }

step "Resolving a native-file-capable selectable model for account '$ACCOUNT_ID'"
api_json GET "/v1/models" "$TMP_DIR/models.json"
if [[ -z "$MODEL_ID" ]]; then
  MODEL_ID="$(python3 - "$TMP_DIR/models.json" "$ACCOUNT_ID" <<'PY'
import json,sys
payload=json.load(open(sys.argv[1],encoding="utf-8"))
account=sys.argv[2]
for model in payload.get("data",[]):
    meta=model.get("llmgateway") or {}
    caps=meta.get("multimodal_capabilities") or {}
    if (
        meta.get("kind")=="route"
        and meta.get("account")==account
        and "file" in (caps.get("input_modalities") or [])
        and caps.get("native_file_upload") is True
        and "application/pdf" in (caps.get("supported_mime_types") or [])
    ):
        print(model.get("id",""))
        break
PY
)"
fi
[[ -n "$MODEL_ID" ]] || {
  echo "FILE ACCEPTANCE FAILED: no native PDF-capable route is selectable for '$ACCOUNT_ID'" >&2
  exit 1
}

python3 - "$TMP_DIR/models.json" "$MODEL_ID" <<'PY'
import json,sys
payload=json.load(open(sys.argv[1],encoding="utf-8"))
model_id=sys.argv[2]
model=next((m for m in payload.get("data",[]) if m.get("id")==model_id),None)
if model is None:
    raise SystemExit(f"FILE ACCEPTANCE FAILED: selected model {model_id!r} is not exposed by /v1/models")
caps=(model.get("llmgateway") or {}).get("multimodal_capabilities") or {}
if "file" not in (caps.get("input_modalities") or []):
    raise SystemExit(f"FILE ACCEPTANCE FAILED: selected model {model_id!r} does not advertise file input: {caps}")
if caps.get("native_file_upload") is not True:
    raise SystemExit(f"FILE ACCEPTANCE FAILED: selected model {model_id!r} does not advertise native file upload: {caps}")
if "application/pdf" not in (caps.get("supported_mime_types") or []):
    raise SystemExit(f"FILE ACCEPTANCE FAILED: selected model {model_id!r} does not advertise PDF support: {caps}")
PY
step "Using model '$MODEL_ID'"

step "Uploading PDF artifact"
curl -fsS -X POST "$BASE_URL/v1/files" \
  -H "Authorization: Bearer $API_KEY" \
  -F 'purpose=assistants' \
  -F "file=@$PDF_PATH;type=application/pdf" \
  -o "$TMP_DIR/file.json"
FILE_ID="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1])).get("id",""))' "$TMP_DIR/file.json")"
[[ -n "$FILE_ID" ]] || { echo "FILE ACCEPTANCE FAILED: /v1/files returned no id" >&2; exit 1; }
step "Stored PDF as '$FILE_ID'"

python3 - "$MODEL_ID" "$FILE_ID" "$TMP_DIR/responses.json" <<'PY'
import json,sys
model,file_id,path=sys.argv[1:4]
body={
  "model":model,
  "stream":False,
  "input":[{
    "type":"message",
    "role":"user",
    "content":[
      {"type":"input_text","text":"Read the attached PDF and reply with one short sentence confirming its subject."},
      {"type":"input_file","file_id":file_id}
    ]
  }]
}
json.dump(body,open(path,"w",encoding="utf-8"))
PY

step "Scenario 1/3: Responses native PDF upload"
curl -fsS -D "$TMP_DIR/responses.headers" -o "$TMP_DIR/responses.out.json" \
  -X POST "$BASE_URL/v1/responses" \
  -H "Authorization: Bearer $API_KEY" \
  -H "Content-Type: application/json" \
  --data-binary "@$TMP_DIR/responses.json"
python3 - "$TMP_DIR/responses.out.json" <<'PY'
import json,sys
x=json.load(open(sys.argv[1],encoding="utf-8"))
texts=[]
for item in x.get("output") or []:
    for part in item.get("content") or []:
        if part.get("type")=="output_text" and str(part.get("text") or "").strip():
            texts.append(part["text"])
if not texts:
    raise SystemExit("FILE ACCEPTANCE FAILED: Responses returned no output_text\n"+json.dumps(x,indent=2))
raw=json.dumps(x)
for forbidden in ("provider_file_id","file_data","llmgateway_artifact_id"):
    if forbidden in raw:
        raise SystemExit(f"FILE ACCEPTANCE FAILED: public Responses payload leaked {forbidden}")
PY
ROUTE_1="$(header_value "$TMP_DIR/responses.headers" "x-llmgateway-route")"
REQUEST_1="$(header_value "$TMP_DIR/responses.headers" "x-llmgateway-request-id")"
[[ -n "$ROUTE_1" ]] || { echo "FILE ACCEPTANCE FAILED: Responses returned no route header" >&2; exit 1; }
[[ -n "$REQUEST_1" ]] || { echo "FILE ACCEPTANCE FAILED: Responses returned no request id" >&2; exit 1; }
api_json GET "/_llmgateway/executions/$REQUEST_1" "$TMP_DIR/responses.trace.json"
json_assert "$TMP_DIR/responses.trace.json" "x.get('attachment_strategy') == 'native_upload'" "Responses did not record native_upload attachment strategy"

sleep 0.4
runtime "$TMP_DIR/runtime-responses.json"
python3 - "$TMP_DIR/runtime-responses.json" "$PROVIDER_KIND" <<'PY'
import json,sys
runtime=json.load(open(sys.argv[1],encoding="utf-8"))
provider=sys.argv[2]
last=runtime.get("last_execution") or {}
if last.get("transport") != "browser-cdp":
    raise SystemExit(f"FILE ACCEPTANCE FAILED: PDF turn used {last.get('transport')!r}, expected browser-cdp: {last}")
expected={"browser-chatgpt":"chatgpt-web","browser-gemini":"gemini-web"}[provider]
if last.get("adapter_id") != expected:
    raise SystemExit(f"FILE ACCEPTANCE FAILED: PDF turn used adapter {last.get('adapter_id')!r}, expected {expected!r}: {last}")
PY

step "Scenario 2/3: Threads persists the same PDF artifact reference"
THREAD_BODY="$(python3 - "$MODEL_ID" <<'PY'
import json,sys
print(json.dumps({"title":"P3 live PDF acceptance","model":sys.argv[1]}))
PY
)"
api_json POST "/v1/threads" "$TMP_DIR/thread-create.json" "$THREAD_BODY"
THREAD_ID="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1])).get("id",""))' "$TMP_DIR/thread-create.json")"
[[ -n "$THREAD_ID" ]] || { echo "FILE ACCEPTANCE FAILED: thread creation returned no id" >&2; exit 1; }

THREAD_MESSAGE="$(python3 - "$MODEL_ID" "$FILE_ID" <<'PY'
import json,sys
print(json.dumps({
  "model":sys.argv[1],
  "stream":False,
  "content":[
    {"type":"input_text","text":"Use this PDF in the persisted thread and answer briefly."},
    {"type":"input_file","file_id":sys.argv[2]}
  ]
}))
PY
)"
api_json POST "/v1/threads/$THREAD_ID/messages" "$TMP_DIR/thread-reply.json" "$THREAD_MESSAGE"
json_assert "$TMP_DIR/thread-reply.json" "bool((((x.get('choices') or [{}])[0].get('message') or {}).get('content')))" "Threads PDF turn returned empty assistant text"

api_json GET "/v1/threads/$THREAD_ID" "$TMP_DIR/thread-detail.json"
python3 - "$TMP_DIR/thread-detail.json" "$FILE_ID" <<'PY'
import json,sys
thread=json.load(open(sys.argv[1],encoding="utf-8"))
file_id=sys.argv[2]
uri="llmgateway://artifact/"+file_id
found=False
raw=json.dumps(thread)
for forbidden in ("provider_file_id","file_data","llmgateway_artifact_id"):
    if forbidden in raw:
        raise SystemExit(f"FILE ACCEPTANCE FAILED: persisted thread leaked {forbidden}")
for row in thread.get("messages") or []:
    content=(row.get("message") or {}).get("content")
    if not isinstance(content,list):
        continue
    for part in content:
        if isinstance(part,dict) and part.get("type")=="input_file" and part.get("file_id")==uri:
            found=True
if not found:
    raise SystemExit(f"FILE ACCEPTANCE FAILED: thread did not persist {uri}")
PY

step "Capturing native conversation affinity after the PDF turn"
affinity "$TMP_DIR/affinity-pdf.json"
json_assert "$TMP_DIR/affinity-pdf.json" "x.get('mapping') is not None and bool(x.get('mapping',{}).get('conversation_url'))" "PDF thread turn did not establish native conversation affinity"
json_assert "$TMP_DIR/affinity-pdf.json" "int(x.get('mapping',{}).get('last_synced_ordinal',0)) > 0" "PDF thread turn did not advance native sync ordinal"
NATIVE_URL="$(json_value "$TMP_DIR/affinity-pdf.json" "str(x.get('mapping',{}).get('conversation_url') or '')")"
SYNC_ORDINAL="$(json_value "$TMP_DIR/affinity-pdf.json" "int(x.get('mapping',{}).get('last_synced_ordinal',0))")"
[[ -n "$NATIVE_URL" && "$SYNC_ORDINAL" -gt 0 ]] || {
  echo "FILE ACCEPTANCE FAILED: invalid native affinity checkpoint after PDF turn" >&2
  exit 1
}

step "Scenario 3/3: Native thread follow-up does not reattach the historical PDF"
FOLLOW_UP='{"stream":false,"content":[{"type":"input_text","text":"Follow up on the same PDF without attaching it again. Answer in one short sentence."}]}'
api_json POST "/v1/threads/$THREAD_ID/messages" "$TMP_DIR/thread-follow-up.json" "$FOLLOW_UP"
json_assert "$TMP_DIR/thread-follow-up.json" "bool((((x.get('choices') or [{}])[0].get('message') or {}).get('content')))" "native thread follow-up returned empty assistant text"

affinity "$TMP_DIR/affinity-follow-up.json"
python3 - "$TMP_DIR/affinity-follow-up.json" "$NATIVE_URL" "$SYNC_ORDINAL" <<'PY'
import json,sys
path,expected_url,previous_ordinal=sys.argv[1:4]
payload=json.load(open(path,encoding="utf-8"))
mapping=payload.get("mapping") or {}
actual_url=str(mapping.get("conversation_url") or "")
actual_ordinal=int(mapping.get("last_synced_ordinal") or 0)
if actual_url != expected_url:
    raise SystemExit(
        "FILE ACCEPTANCE FAILED: text-only follow-up changed native conversation affinity"
    )
if actual_ordinal <= int(previous_ordinal):
    raise SystemExit(
        "FILE ACCEPTANCE FAILED: text-only follow-up did not advance native sync ordinal"
    )
PY
FOLLOW_UP_ORDINAL="$(json_value "$TMP_DIR/affinity-follow-up.json" "int(x.get('mapping',{}).get('last_synced_ordinal',0))")"
step "Native affinity reused; synced ordinal advanced from $SYNC_ORDINAL to $FOLLOW_UP_ORDINAL without a new file in the follow-up request"

step "Verifying reference-safe delete while thread owns the PDF"
STATUS="$(curl -sS -o "$TMP_DIR/delete-in-use.json" -w '%{http_code}' -X DELETE \
  "$BASE_URL/v1/files/$FILE_ID" -H "Authorization: Bearer $API_KEY")"
[[ "$STATUS" == "409" ]] || {
  echo "FILE ACCEPTANCE FAILED: deleting referenced PDF returned HTTP $STATUS instead of 409" >&2
  cat "$TMP_DIR/delete-in-use.json" >&2
  exit 1
}
json_assert "$TMP_DIR/delete-in-use.json" "x.get('error',{}).get('type') == 'artifact_in_use'" "referenced PDF did not return artifact_in_use"

if [[ "$KEEP_THREAD" -eq 0 ]]; then
  api_json DELETE "/v1/threads/$THREAD_ID" "$TMP_DIR/thread-delete.json"
  THREAD_ID=""
fi
if [[ "$KEEP_FILE" -eq 0 && "$KEEP_THREAD" -eq 0 ]]; then
  api_json DELETE "/v1/files/$FILE_ID" "$TMP_DIR/file-delete.json"
  FILE_ID=""
fi

step "Checking post-file transport posture"
sleep 0.7
runtime "$TMP_DIR/runtime-after.json"
python3 - "$RUNTIME_BASELINE" "$TMP_DIR/runtime-after.json" <<'PY'
import json,sys
before=json.load(open(sys.argv[1],encoding="utf-8"))
after=json.load(open(sys.argv[2],encoding="utf-8"))
last=after.get("last_execution") or {}
if last.get("transport") != "browser-cdp":
    raise SystemExit(f"FILE ACCEPTANCE FAILED: final PDF execution telemetry is not browser-cdp: {last}")
if not before.get("browser_running") and before.get("direct_ready"):
    if not last.get("browser_fallback"):
        raise SystemExit(f"FILE ACCEPTANCE FAILED: browserless-preferred PDF turn did not record browser fallback: {last}")
    if after.get("browser_running"):
        raise SystemExit("FILE ACCEPTANCE FAILED: temporary file-upload CDP browser was not released")
PY

step "PASS: live PDF works through Responses and Threads with stable gateway artifact identity"
step "provider=$PROVIDER_KIND account=$ACCOUNT_ID model=$MODEL_ID route=$ROUTE_1"
if [[ "$GENERATED_PDF" -eq 1 ]]; then
  step "acceptance used generated valid PDF fixture"
fi
