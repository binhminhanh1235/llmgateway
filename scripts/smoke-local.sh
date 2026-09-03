#!/usr/bin/env bash
set -euo pipefail

export LLMGATEWAY_API_KEY="ci-local-key"
export LLMGATEWAY_CONFIG="config/llmgateway.example.toml"

rm -f data/llmgateway.db data/llmgateway.db-shm data/llmgateway.db-wal
mkdir -p data

cargo build --quiet
./target/debug/llmgateway >/tmp/llmgateway-smoke.log 2>&1 &
PID=$!
trap 'kill "$PID" 2>/dev/null || true; rm -f data/llmgateway.db data/llmgateway.db-shm data/llmgateway.db-wal' EXIT

for _ in {1..30}; do
  if curl -fsS http://127.0.0.1:7331/_llmgateway/health >/dev/null; then
    break
  fi
  sleep 0.2
done

curl -fsS http://127.0.0.1:7331/ | grep -q "llmgateway"
curl -fsS http://127.0.0.1:7331/ui/app.js | grep -q "llmgateway.threads.v1"
curl -fsS http://127.0.0.1:7331/v1/models \
  -H "Authorization: Bearer ${LLMGATEWAY_API_KEY}" | grep -q "llmgateway-auto"

THREAD_JSON=$(curl -fsS -X POST \
  http://127.0.0.1:7331/v1/threads \
  -H "Authorization: Bearer ${LLMGATEWAY_API_KEY}" \
  -H "Content-Type: application/json" \
  -d '{"title":"CI thread","model":"llmgateway-auto"}')
THREAD_ID=$(printf '%s' "$THREAD_JSON" | python3 -c 'import json,sys; print(json.load(sys.stdin)["id"])')

curl -fsS http://127.0.0.1:7331/v1/threads \
  -H "Authorization: Bearer ${LLMGATEWAY_API_KEY}" | grep -q "$THREAD_ID"
curl -fsS "http://127.0.0.1:7331/v1/threads/${THREAD_ID}" \
  -H "Authorization: Bearer ${LLMGATEWAY_API_KEY}" | grep -q '"title":"CI thread"'
curl -fsS -X DELETE "http://127.0.0.1:7331/v1/threads/${THREAD_ID}" \
  -H "Authorization: Bearer ${LLMGATEWAY_API_KEY}" | grep -q '"deleted":true'

curl -fsS -X PATCH \
  http://127.0.0.1:7331/_llmgateway/accounts/gemini-primary/models \
  -H "Authorization: Bearer ${LLMGATEWAY_API_KEY}" \
  -H "Content-Type: application/json" \
  -d '{"model_id":"gemini/gemini-3.7-flash","enabled":false}' | grep -q '"enabled":false'

curl -fsS \
  http://127.0.0.1:7331/_llmgateway/accounts/gemini-primary/models \
  -H "Authorization: Bearer ${LLMGATEWAY_API_KEY}" | grep -q '"enabled":false'

echo "llmgateway local smoke test passed"
