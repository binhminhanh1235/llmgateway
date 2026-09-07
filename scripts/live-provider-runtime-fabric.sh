#!/usr/bin/env bash
set -euo pipefail

BASE_URL="${LLMGATEWAY_BASE_URL:-http://127.0.0.1:7331}"
API_KEY="${LLMGATEWAY_API_KEY:-}"
GEMINI_ACCOUNT=""
QWEN_ACCOUNT=""
DEEPSEEK_ACCOUNT=""
EVIDENCE_PATH="${LLMGATEWAY_LIVE_EVIDENCE:-}"

usage() {
  cat <<'EOF'
Usage:
  live-provider-runtime-fabric.sh [options]

Options:
  --gemini-account <id>   Optional; auto-discovers one ready Gemini account
  --qwen-account <id>     Optional; auto-discovers one ready Qwen account
  --deepseek-account <id> Optional; auto-discovers one ready DeepSeek account
  --base-url <url>        Gateway URL (default: http://127.0.0.1:7331)
  --api-key <key>         Or set LLMGATEWAY_API_KEY
  --evidence <path>       Write a credential-free JSON PASS summary

If an account id is omitted, the runner inspects enabled browser accounts and
requires exactly one authenticated ready/degraded account for that provider.
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
    --evidence) EVIDENCE_PATH="${2:-}"; shift 2 ;;
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

export LLMGATEWAY_BASE_URL="$BASE_URL"
export LLMGATEWAY_API_KEY="$API_KEY"

discover_account() {
  local provider_kind="$1"
  local label="$2"
  local accounts_file="$TMP_DIR/accounts.json"
  local account_id runtime_file
  local -a matches=()

  if [[ ! -f "$accounts_file" ]]; then
    curl -fsS -H "Authorization: Bearer $API_KEY"       "$BASE_URL/_llmgateway/accounts" -o "$accounts_file"
  fi

  while IFS= read -r account_id; do
    [[ -n "$account_id" ]] || continue
    runtime_file="$TMP_DIR/discover-$(printf '%s' "$account_id" | tr -c 'A-Za-z0-9._-' '_').json"
    if ! curl -fsS -H "Authorization: Bearer $API_KEY"       "$BASE_URL/_llmgateway/browser-accounts/$account_id/runtime"       -o "$runtime_file" 2>/dev/null; then
      continue
    fi
    if python3 - "$runtime_file" "$provider_kind" <<'PY'
import json,sys
x=json.load(open(sys.argv[1],encoding="utf-8"))
expected=sys.argv[2]
session=x.get("session") or {}
ready=session.get("status") in {"ready","degraded"}
raise SystemExit(0 if (
    x.get("provider_kind") == expected
    and ready
    and bool(x.get("auth_snapshot_available"))
) else 1)
PY
    then
      matches+=("$account_id")
    fi
  done < <(python3 - "$accounts_file" <<'PY'
import json,sys
x=json.load(open(sys.argv[1],encoding="utf-8"))
for account in x.get("data",[]):
    if account.get("enabled") and account.get("id"):
        print(account["id"])
PY
)

  if [[ "${#matches[@]}" -eq 1 ]]; then
    printf '%s\n' "${matches[0]}"
    return 0
  fi
  if [[ "${#matches[@]}" -eq 0 ]]; then
    echo "LIVE ACCEPTANCE FAILED: no authenticated ready $label account was auto-discovered" >&2
  else
    echo "LIVE ACCEPTANCE FAILED: multiple authenticated ready $label accounts found: ${matches[*]}; pass the account id explicitly" >&2
  fi
  return 1
}

if [[ -z "$GEMINI_ACCOUNT" ]]; then
  GEMINI_ACCOUNT="$(discover_account browser-gemini Gemini)"
  echo "Auto-discovered Gemini account: $GEMINI_ACCOUNT"
fi
if [[ -z "$QWEN_ACCOUNT" ]]; then
  QWEN_ACCOUNT="$(discover_account browser-qwen Qwen)"
  echo "Auto-discovered Qwen account: $QWEN_ACCOUNT"
fi
if [[ -z "$DEEPSEEK_ACCOUNT" ]]; then
  DEEPSEEK_ACCOUNT="$(discover_account browser-deepseek DeepSeek)"
  echo "Auto-discovered DeepSeek account: $DEEPSEEK_ACCOUNT"
fi

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

python3 - "$TMP_DIR" "$GEMINI_ACCOUNT" "$QWEN_ACCOUNT" "$DEEPSEEK_ACCOUNT" "$EVIDENCE_PATH" "$BASE_URL" <<'PY'
import datetime
import json
import os
import re
import sys

root = sys.argv[1]
accounts = sys.argv[2:5]
evidence_path = sys.argv[5]
base_url = sys.argv[6]
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
account_summaries = {}
for payload in payloads:
    runtime = payload.get("account_runtime") or {}
    account_id = payload.get("account_id")
    print(
        f"Account {account_id}: "
        f"in_flight={runtime.get('in_flight', 0)} "
        f"queue={runtime.get('queue_depth', 0)}/"
        f"{runtime.get('max_queue_depth', 0)} "
        f"limit={runtime.get('concurrency_limit', 0)} "
        f"generation={runtime.get('generation', 0)}"
    )
    account_summaries[account_id] = {
        "provider_kind": payload.get("provider_kind"),
        "session_status": (payload.get("session") or {}).get("status"),
        "effective_transport": payload.get("effective_transport"),
        "direct_ready": bool(payload.get("direct_ready")),
        "browser_running": bool(payload.get("browser_running")),
        "in_flight": int(runtime.get("in_flight") or 0),
        "queue_depth": int(runtime.get("queue_depth") or 0),
        "max_queue_depth": int(runtime.get("max_queue_depth") or 0),
        "concurrency_limit": int(runtime.get("concurrency_limit") or 0),
        "generation": int(runtime.get("generation") or 0),
    }

if evidence_path:
    parent = os.path.dirname(os.path.abspath(evidence_path))
    if parent:
        os.makedirs(parent, exist_ok=True)
    evidence = {
        "status": "pass",
        "verified_at": datetime.datetime.now(datetime.timezone.utc).isoformat(),
        "gateway": base_url,
        "accounts": {
            "gemini": accounts[0],
            "qwen": accounts[1],
            "deepseek": accounts[2],
        },
        "browser_runtime_metrics": {
            key: int(metrics[key]) for key in sorted(required)
        },
        "account_runtime": account_summaries,
    }
    with open(evidence_path, "w", encoding="utf-8") as f:
        json.dump(evidence, f, indent=2, sort_keys=True)
        f.write("\n")
    print(f"Evidence: {evidence_path}")
PY

echo
echo "PROVIDER RUNTIME FABRIC LIVE ACCEPTANCE: PASS"
