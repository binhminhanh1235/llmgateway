#!/usr/bin/env bash
set -euo pipefail

: "${LLMGATEWAY_API_KEY:?LLMGATEWAY_API_KEY is required}"

BASE_URL="${LLMGATEWAY_BASE_URL:-http://127.0.0.1:7331}"
TMP_DIR=$(mktemp -d)
cleanup() { rm -rf "$TMP_DIR"; }
trap cleanup EXIT

assert_json_error() {
  local expected_status="$1"
  local expected_type="$2"
  local actual_status="$3"
  test "$actual_status" = "$expected_status"
  python3 - "$TMP_DIR/error.json" "$expected_type" <<'PY'
import json,sys
with open(sys.argv[1], encoding="utf-8") as f:
    payload=json.load(f)
assert payload["error"]["type"] == sys.argv[2], payload
assert payload["error"]["message"], payload
PY
}

echo "[p3-file-smoke] prepare fixtures"
printf 'P3 extraction fallback marker: alpha beta gamma\n' >"$TMP_DIR/notes.txt"
printf '%%PDF-1.7\nP3 native PDF fixture\n%%%%EOF\n' >"$TMP_DIR/report.pdf"

echo "[p3-file-smoke] upload text fixture"
TEXT_JSON=$(curl -fsS -X POST "$BASE_URL/v1/files" \
  -H "Authorization: Bearer ${LLMGATEWAY_API_KEY}" \
  -F 'purpose=assistants' \
  -F "file=@$TMP_DIR/notes.txt;type=text/plain")
TEXT_ID=$(printf '%s' "$TEXT_JSON" | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')

python3 - "$TEXT_ID" "$TMP_DIR/text-fallback.json" <<'PY'
import json,sys
body={
  "model":"llmgateway-text-only",
  "messages":[{
    "role":"user",
    "content":[
      {"type":"text","text":"Summarize the attached notes"},
      {"type":"input_file","file_id":sys.argv[1]}
    ]
  }]
}
with open(sys.argv[2],"w",encoding="utf-8") as f: json.dump(body,f)
PY

echo "[p3-file-smoke] execute extraction fallback"
TEXT_REPLY=$(curl -fsS -D "$TMP_DIR/text-fallback.headers" -X POST "$BASE_URL/v1/chat/completions" \
  -H "Authorization: Bearer ${LLMGATEWAY_API_KEY}" \
  -H "Content-Type: application/json" \
  --data-binary "@$TMP_DIR/text-fallback.json")
printf '%s' "$TEXT_REPLY" | grep -q 'fake file reply messages=1 extracted_file=yes'
TEXT_REQUEST_ID=$(awk 'BEGIN{IGNORECASE=1} /^x-llmgateway-request-id:/ {gsub("\\r", "", $2); print $2}' "$TMP_DIR/text-fallback.headers")
test -n "$TEXT_REQUEST_ID"
TEXT_TRACE=$(curl -fsS "$BASE_URL/_llmgateway/executions/$TEXT_REQUEST_ID" \
  -H "Authorization: Bearer ${LLMGATEWAY_API_KEY}")
printf '%s' "$TEXT_TRACE" | python3 -c '
import json,sys
x=json.load(sys.stdin)
assert x["attachment_strategy"] == "extracted_fallback", x
'

curl -fsS -X DELETE "$BASE_URL/v1/files/$TEXT_ID" \
  -H "Authorization: Bearer ${LLMGATEWAY_API_KEY}" | grep -q '"deleted":true'

echo "[p3-file-smoke] upload native PDF fixture"
PDF_JSON=$(curl -fsS -X POST "$BASE_URL/v1/files" \
  -H "Authorization: Bearer ${LLMGATEWAY_API_KEY}" \
  -F 'purpose=assistants' \
  -F "file=@$TMP_DIR/report.pdf;type=application/pdf")
PDF_ID=$(printf '%s' "$PDF_JSON" | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')

python3 - "$PDF_ID" "$TMP_DIR/pdf-responses.json" "$TMP_DIR/pdf-text-only.json" <<'PY'
import json,sys
file_id=sys.argv[1]
responses={
  "model":"llmgateway-auto",
  "input":[{
    "type":"message",
    "role":"user",
    "content":[
      {"type":"input_text","text":"Summarize this PDF"},
      {"type":"input_file","file_id":file_id}
    ]
  }]
}
text_only={
  "model":"llmgateway-text-only",
  "messages":[{
    "role":"user",
    "content":[
      {"type":"text","text":"This native PDF route must be rejected"},
      {"type":"input_file","file_id":file_id}
    ]
  }]
}
for path,body in [(sys.argv[2],responses),(sys.argv[3],text_only)]:
    with open(path,"w",encoding="utf-8") as f: json.dump(body,f)
PY

echo "[p3-file-smoke] execute native PDF via Responses"
PDF_RESPONSE=$(curl -fsS -D "$TMP_DIR/pdf-response.headers" -X POST "$BASE_URL/v1/responses" \
  -H "Authorization: Bearer ${LLMGATEWAY_API_KEY}" \
  -H "Content-Type: application/json" \
  --data-binary "@$TMP_DIR/pdf-responses.json")
printf '%s' "$PDF_RESPONSE" | grep -q 'fake file reply messages=1 native_file=yes'
PDF_REQUEST_ID=$(awk 'BEGIN{IGNORECASE=1} /^x-llmgateway-request-id:/ {gsub("\\r", "", $2); print $2}' "$TMP_DIR/pdf-response.headers")
test -n "$PDF_REQUEST_ID"
PDF_TRACE=$(curl -fsS "$BASE_URL/_llmgateway/executions/$PDF_REQUEST_ID" \
  -H "Authorization: Bearer ${LLMGATEWAY_API_KEY}")
printf '%s' "$PDF_TRACE" | python3 -c '
import json,sys
x=json.load(sys.stdin)
assert x["attachment_strategy"] == "native_upload", x
'
printf '%s' "$PDF_RESPONSE" | python3 -c '
import json,sys
x=json.load(sys.stdin)
raw=json.dumps(x)
assert "provider_file_id" not in raw, x
assert "file_data" not in raw, x
'

STATUS=$(curl -sS -o "$TMP_DIR/error.json" -w '%{http_code}' -X POST "$BASE_URL/v1/chat/completions" \
  -H "Authorization: Bearer ${LLMGATEWAY_API_KEY}" \
  -H "Content-Type: application/json" \
  --data-binary "@$TMP_DIR/pdf-text-only.json")
assert_json_error 400 unsupported_capability "$STATUS"

# Responses persist gateway artifact references. Deletion must be guarded.
STATUS=$(curl -sS -o "$TMP_DIR/error.json" -w '%{http_code}' -X DELETE "$BASE_URL/v1/files/$PDF_ID" \
  -H "Authorization: Bearer ${LLMGATEWAY_API_KEY}")
assert_json_error 409 artifact_in_use "$STATUS"

echo "[p3-file-smoke] execute native PDF via Threads"
THREAD_PDF_JSON=$(curl -fsS -X POST "$BASE_URL/v1/files" \
  -H "Authorization: Bearer ${LLMGATEWAY_API_KEY}" \
  -F 'purpose=assistants' \
  -F "file=@$TMP_DIR/report.pdf;type=application/pdf")
THREAD_PDF_ID=$(printf '%s' "$THREAD_PDF_JSON" | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')

THREAD_JSON=$(curl -fsS -X POST "$BASE_URL/v1/threads" \
  -H "Authorization: Bearer ${LLMGATEWAY_API_KEY}" \
  -H "Content-Type: application/json" \
  -d '{"title":"P3 PDF thread","model":"llmgateway-auto"}')
THREAD_ID=$(printf '%s' "$THREAD_JSON" | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')

python3 - "$THREAD_PDF_ID" "$TMP_DIR/thread-file.json" <<'PY'
import json,sys
body={
  "content":[
    {"type":"input_text","text":"Use this PDF in the thread"},
    {"type":"input_file","file_id":sys.argv[1]}
  ],
  "stream":False
}
with open(sys.argv[2],"w",encoding="utf-8") as f: json.dump(body,f)
PY

THREAD_REPLY=$(curl -fsS -X POST "$BASE_URL/v1/threads/$THREAD_ID/messages" \
  -H "Authorization: Bearer ${LLMGATEWAY_API_KEY}" \
  -H "Content-Type: application/json" \
  --data-binary "@$TMP_DIR/thread-file.json")
printf '%s' "$THREAD_REPLY" | grep -q 'fake file reply messages=1 native_file=yes'

THREAD_DETAIL=$(curl -fsS "$BASE_URL/v1/threads/$THREAD_ID" \
  -H "Authorization: Bearer ${LLMGATEWAY_API_KEY}")
printf '%s' "$THREAD_DETAIL" > "$TMP_DIR/thread-detail.json"
python3 - "$THREAD_PDF_ID" "$TMP_DIR/thread-detail.json" <<'PY'
import json,sys
file_id=sys.argv[1]
with open(sys.argv[2], encoding="utf-8") as f:
    x=json.load(f)
content=x["messages"][0]["message"]["content"]
part=next(item for item in content if item.get("type")=="input_file")
assert part["file_id"] == "llmgateway://artifact/" + file_id, x
raw=json.dumps(x)
assert "provider_file_id" not in raw, x
assert "file_data" not in raw, x
PY

STATUS=$(curl -sS -o "$TMP_DIR/error.json" -w '%{http_code}' -X DELETE "$BASE_URL/v1/files/$THREAD_PDF_ID" \
  -H "Authorization: Bearer ${LLMGATEWAY_API_KEY}")
assert_json_error 409 artifact_in_use "$STATUS"

curl -fsS -X DELETE "$BASE_URL/v1/threads/$THREAD_ID" \
  -H "Authorization: Bearer ${LLMGATEWAY_API_KEY}" | grep -q '"deleted":true'
curl -fsS -X DELETE "$BASE_URL/v1/files/$THREAD_PDF_ID" \
  -H "Authorization: Bearer ${LLMGATEWAY_API_KEY}" | grep -q '"deleted":true'

python3 - "$TMP_DIR/remote-file.json" <<'PY'
import json,sys
body={
  "model":"llmgateway-auto",
  "messages":[{
    "role":"user",
    "content":[{"type":"input_file","file_url":"https://example.invalid/report.pdf"}]
  }]
}
with open(sys.argv[1],"w",encoding="utf-8") as f: json.dump(body,f)
PY
STATUS=$(curl -sS -o "$TMP_DIR/error.json" -w '%{http_code}' -X POST "$BASE_URL/v1/chat/completions" \
  -H "Authorization: Bearer ${LLMGATEWAY_API_KEY}" \
  -H "Content-Type: application/json" \
  --data-binary "@$TMP_DIR/remote-file.json")
assert_json_error 400 unsupported_capability "$STATUS"

echo "[p3-file-smoke] verify capability metadata"
CAPS=$(curl -fsS "$BASE_URL/v1/capabilities" -H "Authorization: Bearer ${LLMGATEWAY_API_KEY}")
printf '%s' "$CAPS" | python3 -c '
import json,sys
x=json.load(sys.stdin)
g=x["gateway_execution"]
assert "file" in g["input_modalities"], g
assert g["native_file_upload"] is True, g
for mime in ["application/pdf","text/plain","text/markdown","text/csv","application/json","application/vnd.openxmlformats-officedocument.wordprocessingml.document"]:
    assert mime in g["supported_mime_types"], (mime,g)
'

echo "P3 file attachment API/Threads smoke passed"
