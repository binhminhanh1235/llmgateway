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

python3 - "$TMP_DIR/vision.png" <<'PY'
import sys
with open(sys.argv[1], "wb") as f:
    f.write(b"\x89PNG\r\n\x1a\nvision-smoke")
PY

INLINE_DATA=$(python3 - "$TMP_DIR/vision.png" <<'PY'
import base64,sys
print("data:image/png;base64," + base64.b64encode(open(sys.argv[1],"rb").read()).decode())
PY
)

python3 - "$INLINE_DATA" "$TMP_DIR/chat-inline.json" <<'PY'
import json,sys
body={
  "model":"llmgateway-auto",
  "messages":[{
    "role":"user",
    "content":[
      {"type":"text","text":"Describe this inline image"},
      {"type":"image_url","image_url":{"url":sys.argv[1]}}
    ]
  }]
}
with open(sys.argv[2],"w",encoding="utf-8") as f: json.dump(body,f)
PY

INLINE_REPLY=$(curl -fsS -X POST "$BASE_URL/v1/chat/completions" \
  -H "Authorization: Bearer ${LLMGATEWAY_API_KEY}" \
  -H "Content-Type: application/json" \
  --data-binary "@$TMP_DIR/chat-inline.json")
printf '%s' "$INLINE_REPLY" | grep -q 'fake vision reply messages=1 image=yes'

FILE_JSON=$(curl -fsS -X POST "$BASE_URL/v1/files" \
  -H "Authorization: Bearer ${LLMGATEWAY_API_KEY}" \
  -F 'purpose=vision' \
  -F "file=@$TMP_DIR/vision.png;type=image/png")
FILE_ID=$(printf '%s' "$FILE_JSON" | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')

python3 - "$FILE_ID" "$TMP_DIR/responses.json" "$TMP_DIR/text-only.json" <<'PY'
import json,sys
file_id=sys.argv[1]
responses={
  "model":"llmgateway-auto",
  "input":[{
    "type":"message",
    "role":"user",
    "content":[
      {"type":"input_text","text":"Reuse the stored image"},
      {"type":"input_image","file_id":file_id}
    ]
  }]
}
text_only={
  "model":"llmgateway-text-only",
  "messages":[{
    "role":"user",
    "content":[
      {"type":"text","text":"This route must reject vision"},
      {"type":"image_url","file_id":file_id}
    ]
  }]
}
for path,body in [(sys.argv[2],responses),(sys.argv[3],text_only)]:
    with open(path,"w",encoding="utf-8") as f: json.dump(body,f)
PY

RESPONSES_REPLY=$(curl -fsS -X POST "$BASE_URL/v1/responses" \
  -H "Authorization: Bearer ${LLMGATEWAY_API_KEY}" \
  -H "Content-Type: application/json" \
  --data-binary "@$TMP_DIR/responses.json")
printf '%s' "$RESPONSES_REPLY" | grep -q 'fake vision reply messages=1 image=yes'

STATUS=$(curl -sS -o "$TMP_DIR/error.json" -w '%{http_code}' -X POST "$BASE_URL/v1/chat/completions" \
  -H "Authorization: Bearer ${LLMGATEWAY_API_KEY}" \
  -H "Content-Type: application/json" \
  --data-binary "@$TMP_DIR/text-only.json")
assert_json_error 400 unsupported_capability "$STATUS"

THREAD_JSON=$(curl -fsS -X POST "$BASE_URL/v1/threads" \
  -H "Authorization: Bearer ${LLMGATEWAY_API_KEY}" \
  -H "Content-Type: application/json" \
  -d '{"title":"Vision smoke thread","model":"llmgateway-auto"}')
THREAD_ID=$(printf '%s' "$THREAD_JSON" | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')

python3 - "$FILE_ID" "$TMP_DIR/thread-message.json" <<'PY'
import json,sys
body={
  "content":[
    {"type":"input_text","text":"Use the same stored image in this thread"},
    {"type":"input_image","file_id":sys.argv[1]}
  ],
  "stream":False
}
with open(sys.argv[2],"w",encoding="utf-8") as f: json.dump(body,f)
PY

THREAD_REPLY=$(curl -fsS -X POST "$BASE_URL/v1/threads/$THREAD_ID/messages" \
  -H "Authorization: Bearer ${LLMGATEWAY_API_KEY}" \
  -H "Content-Type: application/json" \
  --data-binary "@$TMP_DIR/thread-message.json")
printf '%s' "$THREAD_REPLY" | grep -q 'fake vision reply messages=1 image=yes'

THREAD_DETAIL=$(curl -fsS "$BASE_URL/v1/threads/$THREAD_ID" \
  -H "Authorization: Bearer ${LLMGATEWAY_API_KEY}")
printf '%s' "$THREAD_DETAIL" > "$TMP_DIR/thread-detail.json"
python3 - "$FILE_ID" "$TMP_DIR/thread-detail.json" <<'PY'
import json,sys
file_id=sys.argv[1]
with open(sys.argv[2], encoding="utf-8") as f:
    x=json.load(f)
content=x["messages"][0]["message"]["content"]
image=next(part for part in content if part.get("type")=="image_url")
url=image["image_url"]["url"]
assert url == "llmgateway://artifact/" + file_id, x
assert not url.startswith("data:"), x
PY

STATUS=$(curl -sS -o "$TMP_DIR/error.json" -w '%{http_code}' -X DELETE "$BASE_URL/v1/files/$FILE_ID" \
  -H "Authorization: Bearer ${LLMGATEWAY_API_KEY}")
assert_json_error 409 artifact_in_use "$STATUS"

curl -fsS -X DELETE "$BASE_URL/v1/threads/$THREAD_ID" \
  -H "Authorization: Bearer ${LLMGATEWAY_API_KEY}" | grep -q '"deleted":true'
curl -fsS -X DELETE "$BASE_URL/v1/files/$FILE_ID" \
  -H "Authorization: Bearer ${LLMGATEWAY_API_KEY}" | grep -q '"deleted":true'

echo "P2 vision API/Threads smoke passed"
