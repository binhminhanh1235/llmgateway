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
command -v curl >/dev/null || { echo "curl is required" >&2; exit 2; }
command -v python3 >/dev/null || { echo "python3 is required" >&2; exit 2; }

BASE_URL="${BASE_URL%/}"
TMP_DIR="$(mktemp -d)"
trap 'rm -rf "$TMP_DIR"' EXIT

export LLMGATEWAY_BASE_URL="$BASE_URL"
export LLMGATEWAY_API_KEY="$API_KEY"

echo "==> Gemini authenticated runtime acceptance"
bash scripts/live-browserless-acceptance.sh --account "$GEMINI_ACCOUNT"

echo "==> Qwen authenticated runtime acceptance"
bash scripts/live-qwen-browserless-acceptance.sh --account "$QWEN_ACCOUNT"

echo "==> DeepSeek authenticated runtime acceptance"
bash scripts/live-deepseek-provider-runtime.sh --account "$DEEPSEEK_ACCOUNT"

echo "==> Runtime resource/admission summary"
for ACCOUNT in "$GEMINI_ACCOUNT" "$QWEN_ACCOUNT" "$DEEPSEEK_ACCOUNT"; do
  SAFE_NAME="$(printf '%s' "$ACCOUNT" | tr -c 'A-Za-z0-9._-' '_')"
  curl -fsS     -H "Authorization: Bearer $API_KEY"     "$BASE_URL/_llmgateway/browser-accounts/$ACCOUNT/runtime"     -o "$TMP_DIR/$SAFE_NAME.json"
done

python3 - "$TMP_DIR" "$GEMINI_ACCOUNT" "$QWEN_ACCOUNT" "$DEEPSEEK_ACCOUNT" <<'PY'
import json
import os
import re
import sys

root = sys.argv[1]
accounts = sys.argv[2:]
payloads = []
for account in accounts:
    safe = re.sub(r"[^A-Za-z0-9._-]", "_", account)
    with open(os.path.join(root, safe + ".json"), encoding="utf-8") as f:
        payload = json.load(f)
    runtime = payload.get("account_runtime") or {}
    if int(runtime.get("in_flight") or 0) != 0:
        raise SystemExit(f"LIVE ACCEPTANCE FAILED: {account} leaked in-flight admission: {runtime}")
    if int(runtime.get("queue_depth") or 0) != 0:
        raise SystemExit(f"LIVE ACCEPTANCE FAILED: {account} leaked queued work: {runtime}")
    payloads.append(payload)

metrics = payloads[-1].get("browser_runtime_metrics") or {}
required = {"running_browsers", "active_leases", "launch_count", "reclaim_count"}
if not required.issubset(metrics):
    raise SystemExit(f"LIVE ACCEPTANCE FAILED: browser runtime metrics missing: {metrics}")
if int(metrics["running_browsers"]) != 0:
    raise SystemExit(f"LIVE ACCEPTANCE FAILED: browser processes remain active: {metrics}")
if int(metrics["active_leases"]) != 0:
    raise SystemExit(f"LIVE ACCEPTANCE FAILED: browser leases remain active: {metrics}")

print(
    "Resource metrics: "
    f"running={metrics['running_browsers']} "
    f"leases={metrics['active_leases']} "
    f"launches={metrics['launch_count']} "
    f"reclaims={metrics['reclaim_count']}"
)
for payload in payloads:
    runtime = payload.get("account_runtime") or {}
    print(
        f"Account {payload.get('account_id')}: "
        f"in_flight={runtime.get('in_flight', 0)} "
        f"queue={runtime.get('queue_depth', 0)}/"
        f"{runtime.get('max_queue_depth', 0)} "
        f"limit={runtime.get('concurrency_limit', 0)} "
        f"generation={runtime.get('generation', 0)}"
    )
PY

echo
echo "PROVIDER RUNTIME FABRIC LIVE ACCEPTANCE: PASS"
