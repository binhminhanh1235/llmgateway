#!/usr/bin/env bash
set -euo pipefail

BASE_URL="${LLMGATEWAY_BASE_URL:-http://127.0.0.1:7331}"
API_KEY="${LLMGATEWAY_API_KEY:-}"
GEMINI_ACCOUNT=""
QWEN_ACCOUNT=""
DEEPSEEK_ACCOUNT=""

usage() {
  cat <<'EOF'
Usage:
  live-provider-runtime-fabric.sh \
    --gemini-account <id> \
    --qwen-account <id> \
    --deepseek-account <id> [--base-url <url>] [--api-key <key>]

Runs conservative authenticated live acceptance for Provider Runtime Fabric.
It does not bypass CAPTCHA/WAF/provider controls and does not generate a blind
40-request burst; burst behavior is covered deterministically in CI.
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --gemini-account) GEMINI_ACCOUNT="${2:-}"; shift 2 ;;
    --qwen-account) QWEN_ACCOUNT="${2:-}"; shift 2 ;;
    --deepseek-account) DEEPSEEK_ACCOUNT="${2:-}"; shift 2 ;;
    --base-url) BASE_URL="${2:-}"; shift 2 ;;
    --api-key) API_KEY="${2:-}"; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "Unknown argument: $1" >&2; usage >&2; exit 2 ;;
  esac
done

[[ -n "$GEMINI_ACCOUNT" ]] || { echo "--gemini-account is required" >&2; exit 2; }
[[ -n "$QWEN_ACCOUNT" ]] || { echo "--qwen-account is required" >&2; exit 2; }
[[ -n "$DEEPSEEK_ACCOUNT" ]] || { echo "--deepseek-account is required" >&2; exit 2; }
[[ -n "$API_KEY" ]] || { echo "--api-key is required or set LLMGATEWAY_API_KEY" >&2; exit 2; }

export LLMGATEWAY_BASE_URL="$BASE_URL"
export LLMGATEWAY_API_KEY="$API_KEY"

echo "==> Gemini authenticated runtime acceptance"
bash scripts/live-browserless-acceptance.sh --account "$GEMINI_ACCOUNT"

echo "==> Qwen authenticated runtime acceptance"
bash scripts/live-qwen-browserless-acceptance.sh --account "$QWEN_ACCOUNT"

echo "==> DeepSeek authenticated runtime acceptance"
bash scripts/live-deepseek-provider-runtime.sh --account "$DEEPSEEK_ACCOUNT"

echo
echo "PROVIDER RUNTIME FABRIC LIVE ACCEPTANCE: PASS"
