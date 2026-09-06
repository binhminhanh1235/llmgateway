#!/usr/bin/env bash
set -euo pipefail

BASE_URL="${LLMGATEWAY_BASE_URL:-http://127.0.0.1:7331}"
API_KEY="${LLMGATEWAY_API_KEY:-}"
BROWSER_ACCOUNT="${LLMGATEWAY_BROWSER_ACCOUNT:-}"
VISION_MODEL="${LLMGATEWAY_VISION_MODEL:-}"
FILE_MODEL="${LLMGATEWAY_FILE_MODEL:-}"
AUDIO_PATH=""

usage() {
  cat <<'EOF'
Usage:
  live-multimodal-acceptance.sh --browser-account <id> [options]

Runs the complete local multimodal acceptance in one command:
  1. authenticated browser vision
  2. authenticated native-PDF/file replay + lifecycle
  3. audio transcription
  4. image generation + Responses image output + image edit

Options:
  --browser-account <id>  Authenticated ChatGPT/Gemini account used by vision/PDF gates
  --base-url <url>
  --api-key <key>
  --vision-model <id>
  --file-model <id>
  --audio <path>          Optional speech file for transcription
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --browser-account) BROWSER_ACCOUNT="${2:-}"; shift 2 ;;
    --base-url) BASE_URL="${2:-}"; shift 2 ;;
    --api-key) API_KEY="${2:-}"; shift 2 ;;
    --vision-model) VISION_MODEL="${2:-}"; shift 2 ;;
    --file-model) FILE_MODEL="${2:-}"; shift 2 ;;
    --audio) AUDIO_PATH="${2:-}"; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "Unknown argument: $1" >&2; usage >&2; exit 2 ;;
  esac
done

[[ -n "$API_KEY" ]] || { echo "--api-key is required or set LLMGATEWAY_API_KEY" >&2; exit 2; }
[[ -n "$BROWSER_ACCOUNT" ]] || { echo "--browser-account is required or set LLMGATEWAY_BROWSER_ACCOUNT" >&2; exit 2; }
BASE_URL="${BASE_URL%/}"

echo "[multimodal-live] checking gateway"
curl -fsS "$BASE_URL/_llmgateway/health" >/dev/null

VISION_ARGS=(--account "$BROWSER_ACCOUNT" --base-url "$BASE_URL" --api-key "$API_KEY")
[[ -z "$VISION_MODEL" ]] || VISION_ARGS+=(--model "$VISION_MODEL")
echo "[multimodal-live] 1/3 authenticated vision"
bash scripts/live-vision-acceptance.sh "${VISION_ARGS[@]}"

FILE_ARGS=(--account "$BROWSER_ACCOUNT" --base-url "$BASE_URL" --api-key "$API_KEY")
[[ -z "$FILE_MODEL" ]] || FILE_ARGS+=(--model "$FILE_MODEL")
echo "[multimodal-live] 2/3 authenticated native PDF/file"
bash scripts/live-file-acceptance.sh "${FILE_ARGS[@]}"

MEDIA_ARGS=(--base-url "$BASE_URL" --api-key "$API_KEY")
[[ -z "$AUDIO_PATH" ]] || MEDIA_ARGS+=(--audio "$AUDIO_PATH")
echo "[multimodal-live] 3/3 transcription + image generation/editing"
bash scripts/live-media-acceptance.sh "${MEDIA_ARGS[@]}"

echo "[multimodal-live] FULL LOCAL MULTIMODAL ACCEPTANCE PASS"
