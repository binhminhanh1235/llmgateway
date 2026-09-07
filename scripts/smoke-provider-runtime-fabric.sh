#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

run() {
  local label="$1"
  shift
  printf '\n==> %s\n' "$label"
  local started
  started="$(date +%s)"
  "$@"
  local finished
  finished="$(date +%s)"
  printf '<== %s PASS (%ss)\n' "$label" "$((finished - started))"
}

# P8 deterministic acceptance is deliberately composed from the same focused
# suites that CI runs independently. This keeps fault ownership close to the
# layer under test while giving developers one local command for the fabric.
run "adapter fault taxonomy (WAF/auth/provider drift)"   node scripts/test-browser-adapter-fixtures.mjs
run "Gemini throttling/auth recovery"   bash scripts/smoke-gemini-recovery.sh
run "canonical conversation + provider/account failover"   bash scripts/smoke-provider-conversation-affinity.sh
run "invisible runtime + concurrent cold start + resource reclaim"   bash scripts/smoke-invisible-browser-runtime.sh
run "CDP reset/page loss/browser restart recovery"   bash scripts/smoke-browser-reliability.sh
run "stream commit barrier + cancellation"   bash scripts/smoke-browser-streaming.sh
run "typed attempt/fallback trace"   bash scripts/smoke-execution-trace.sh
run "virtual-model continuity and route health"   bash scripts/smoke-model-groups.sh

cat <<'EOF'

PROVIDER RUNTIME FABRIC DETERMINISTIC ACCEPTANCE: PASS

Covered by the suites above plus Rust unit tests run by CI:
- Qwen WAF/direct transport rejection and transport isolation
- Gemini 429/503/auth readiness and bounded admission
- DeepSeek empty/dropped stream recovery and conversation epochs
- direct transport unavailable -> browser fetch/UI fallback
- CDP reset, page loss and browser restart
- partial committed streams and client cancellation
- concurrent cold browser startup and idle reclaim
- canonical local context across provider/account failover
- all-primary-unavailable / virtual-model fallback behavior
EOF
