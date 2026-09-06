#!/usr/bin/env bash
set -euo pipefail
BASE_URL="${LLMGATEWAY_BASE_URL:-http://127.0.0.1:7331}"
KEY="${LLMGATEWAY_API_KEY:?LLMGATEWAY_API_KEY is required}"
MODEL="${LLMGATEWAY_MEDIA_MODEL:-llmgateway-media}"
WAV=/tmp/llmgateway-media.wav
PNG=/tmp/llmgateway-media.png
trap 'rm -f "$WAV" "$PNG"' EXIT

python3 - "$WAV" <<'PY'
import sys
open(sys.argv[1],"wb").write(b"RIFF"+(16).to_bytes(4,"little")+b"WAVEfmt "+b"\0"*16)
PY

curl -fsS -X POST "$BASE_URL/v1/audio/transcriptions"   -H "Authorization: Bearer $KEY"   -F "model=$MODEL"   -F "file=@$WAV;type=audio/wav"   | python3 -c 'import json,sys; x=json.load(sys.stdin); assert "fake transcription" in x["text"], x'

GEN=$(curl -fsS -X POST "$BASE_URL/v1/images/generations"   -H "Authorization: Bearer $KEY"   -H "Content-Type: application/json"   -d "{\"model\":\"$MODEL\",\"prompt\":\"draw a deterministic sunrise\",\"n\":1}")
ID=$(printf '%s' "$GEN" | python3 -c 'import json,sys; x=json.load(sys.stdin); i=x["data"][0]; assert i["mime_type"]=="image/png"; assert "b64_json" not in i; print(i["file_id"])')
curl -fsS "$BASE_URL/v1/files/$ID/content" -H "Authorization: Bearer $KEY" -o "$PNG"
python3 - "$PNG" <<'PY'
import sys
assert open(sys.argv[1],"rb").read().startswith(b"\x89PNG\r\n\x1a\n")
PY

RESP=$(curl -fsS -X POST "$BASE_URL/v1/responses"   -H "Authorization: Bearer $KEY"   -H "Content-Type: application/json"   -d "{\"model\":\"$MODEL\",\"input\":\"draw a gateway icon\",\"output_modalities\":[\"image\"]}")
printf '%s' "$RESP" | python3 -c 'import json,sys; x=json.load(sys.stdin); assert x["status"]=="completed"; assert x["output"][0]["type"]=="output_image"; assert "provider_file_id" not in str(x)'

curl -fsS -X POST "$BASE_URL/v1/images/edits"   -H "Authorization: Bearer $KEY"   -F "model=$MODEL"   -F "prompt=make it brighter"   -F "image=@$PNG;type=image/png"   | python3 -c 'import json,sys; x=json.load(sys.stdin); assert x["data"][0]["file_id"].startswith("file_")'

curl -fsS -X POST "$BASE_URL/_llmgateway/routes/explain"   -H "Authorization: Bearer $KEY"   -H "Content-Type: application/json"   -d '{"model":"llmgateway-text-only","task":"image_generation"}'   | python3 -c 'import json,sys; x=json.load(sys.stdin); c=x["candidates"][0]; assert x["required_capabilities"]==["image_output"]; assert "capability_missing:image_output" in c["exclusion_reasons"]'

curl -fsS -X POST "$BASE_URL/_llmgateway/routes/explain"   -H "Authorization: Bearer $KEY"   -H "Content-Type: application/json"   -d "{\"model\":\"$MODEL\",\"task\":\"image_editing\"}"   | python3 -c 'import json,sys; x=json.load(sys.stdin); assert x["selected_route"]=="fake-media-route"; assert x["required_capabilities"]==["image_output","image_editing"]'

echo "multimodal media API smoke passed"
