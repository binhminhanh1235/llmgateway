#!/usr/bin/env bash
set -euo pipefail

BASE_URL="${LLMGATEWAY_BASE_URL:-http://127.0.0.1:7331}"
API_KEY="${LLMGATEWAY_API_KEY:-}"
TRANSCRIPTION_MODEL="${LLMGATEWAY_TRANSCRIPTION_MODEL:-}"
IMAGE_MODEL="${LLMGATEWAY_IMAGE_MODEL:-}"
EDIT_MODEL="${LLMGATEWAY_EDIT_MODEL:-}"
AUDIO_PATH=""

usage() {
  cat <<'EOF'
Usage:
  live-media-acceptance.sh [options]

Options:
  --base-url <url>
  --api-key <key>
  --transcription-model <id>
  --image-model <id>
  --edit-model <id>
  --audio <path>               Optional real speech file. If omitted, a valid short WAV fixture is used.
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --base-url) BASE_URL="${2:-}"; shift 2 ;;
    --api-key) API_KEY="${2:-}"; shift 2 ;;
    --transcription-model) TRANSCRIPTION_MODEL="${2:-}"; shift 2 ;;
    --image-model) IMAGE_MODEL="${2:-}"; shift 2 ;;
    --edit-model) EDIT_MODEL="${2:-}"; shift 2 ;;
    --audio) AUDIO_PATH="${2:-}"; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "Unknown argument: $1" >&2; usage >&2; exit 2 ;;
  esac
done

[[ -n "$API_KEY" ]] || { echo "--api-key is required or set LLMGATEWAY_API_KEY" >&2; exit 2; }
command -v curl >/dev/null || { echo "curl is required" >&2; exit 2; }
command -v python3 >/dev/null || { echo "python3 is required" >&2; exit 2; }
BASE_URL="${BASE_URL%/}"
TMP_DIR="$(mktemp -d)"
trap 'rm -rf "$TMP_DIR"' EXIT

MODELS_JSON="$TMP_DIR/models.json"
curl -fsS "$BASE_URL/v1/models" -H "Authorization: Bearer $API_KEY" -o "$MODELS_JSON"

resolve_model() {
  local capability="$1" explicit="$2"
  if [[ -n "$explicit" ]]; then
    printf '%s' "$explicit"
    return
  fi
  python3 - "$MODELS_JSON" "$capability" <<'PY'
import json,sys
data=json.load(open(sys.argv[1],encoding="utf-8"))["data"]
cap=sys.argv[2]
for item in data:
    llm=item.get("llmgateway") or {}
    if llm.get("kind") == "route":
        continue
    caps=llm.get("multimodal_capabilities") or {}
    if caps.get(cap) is True:
        print(item["id"])
        raise SystemExit
raise SystemExit("no enabled model advertises "+cap)
PY
}

TRANSCRIPTION_MODEL="$(resolve_model audio_transcription "$TRANSCRIPTION_MODEL")"
IMAGE_MODEL="$(resolve_model image_generation "$IMAGE_MODEL")"
EDIT_MODEL="$(resolve_model image_editing "$EDIT_MODEL")"

echo "[media-live] transcription model: $TRANSCRIPTION_MODEL"
echo "[media-live] image model: $IMAGE_MODEL"
echo "[media-live] edit model: $EDIT_MODEL"

if [[ -z "$AUDIO_PATH" ]]; then
  AUDIO_PATH="$TMP_DIR/fixture.wav"
  python3 - "$AUDIO_PATH" <<'PY'
import math,struct,sys,wave
rate=16000
with wave.open(sys.argv[1],"wb") as w:
    w.setnchannels(1); w.setsampwidth(2); w.setframerate(rate)
    samples=[]
    for i in range(rate):
        value=int(4000*math.sin(2*math.pi*440*i/rate))
        samples.append(struct.pack("<h",value))
    w.writeframes(b"".join(samples))
PY
fi
[[ -f "$AUDIO_PATH" ]] || { echo "audio file not found: $AUDIO_PATH" >&2; exit 2; }

TRANSCRIPTION="$TMP_DIR/transcription.json"
curl -fsS -X POST "$BASE_URL/v1/audio/transcriptions"   -H "Authorization: Bearer $API_KEY"   -F "model=$TRANSCRIPTION_MODEL"   -F "file=@$AUDIO_PATH"   -o "$TRANSCRIPTION"
python3 - "$TRANSCRIPTION" <<'PY'
import json,sys
x=json.load(open(sys.argv[1],encoding="utf-8"))
assert isinstance(x.get("text"),str), x
assert "provider_file_id" not in str(x), x
PY

GEN="$TMP_DIR/generation.json"
curl -fsS -X POST "$BASE_URL/v1/images/generations"   -H "Authorization: Bearer $API_KEY"   -H "Content-Type: application/json"   -d "{"model":"$IMAGE_MODEL","prompt":"A small sunrise over geometric mountains, clean test image","n":1}"   -o "$GEN"
IMAGE_ID="$(python3 - "$GEN" <<'PY'
import json,sys
x=json.load(open(sys.argv[1],encoding="utf-8"))
item=x["data"][0]
assert item["file_id"].startswith("file_"), item
assert item["mime_type"].startswith("image/"), item
assert "provider_file_id" not in str(x), x
print(item["file_id"])
PY
)"

IMAGE_PATH="$TMP_DIR/generated-image"
curl -fsS "$BASE_URL/v1/files/$IMAGE_ID/content"   -H "Authorization: Bearer $API_KEY" -o "$IMAGE_PATH"
test -s "$IMAGE_PATH"

RESP="$TMP_DIR/response-image.json"
curl -fsS -X POST "$BASE_URL/v1/responses"   -H "Authorization: Bearer $API_KEY"   -H "Content-Type: application/json"   -d "{"model":"$IMAGE_MODEL","input":"Create a compact blue gateway icon","output_modalities":["image"]}"   -o "$RESP"
python3 - "$RESP" <<'PY'
import json,sys
x=json.load(open(sys.argv[1],encoding="utf-8"))
assert x["status"]=="completed",x
assert any(item.get("type")=="output_image" and item.get("file_id","").startswith("file_") for item in x["output"]),x
assert "provider_file_id" not in str(x),x
PY

EDIT="$TMP_DIR/edit.json"
curl -fsS -X POST "$BASE_URL/v1/images/edits"   -H "Authorization: Bearer $API_KEY"   -F "model=$EDIT_MODEL"   -F "prompt=Add a subtle warm glow"   -F "image=@$IMAGE_PATH;type=image/png"   -o "$EDIT"
python3 - "$EDIT" <<'PY'
import json,sys
x=json.load(open(sys.argv[1],encoding="utf-8"))
assert x["data"][0]["file_id"].startswith("file_"),x
assert "provider_file_id" not in str(x),x
PY

echo "[media-live] PASS: transcription + image generation + Responses image output + image edit"
