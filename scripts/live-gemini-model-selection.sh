#!/usr/bin/env bash
set -euo pipefail

BASE_URL="${LLMGATEWAY_BASE_URL:-http://127.0.0.1:7331}"
API_KEY="${LLMGATEWAY_API_KEY:-}"
ACCOUNT_ID=""
MODEL_A=""
MODEL_B=""
KEEP_THREADS=0
THREADS=()
TMP_DIR="$(mktemp -d)"

usage() {
  cat <<'EOF'
Usage:
  live-gemini-model-selection.sh --account <id> [options]

Options:
  --base-url <url>
  --api-key <key>       Or set LLMGATEWAY_API_KEY
  --model-a <id>        Canonical or external discovered model id
  --model-b <id>        Canonical or external discovered model id
  --keep-threads
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --account) ACCOUNT_ID="${2:-}"; shift 2 ;;
    --base-url) BASE_URL="${2:-}"; shift 2 ;;
    --api-key) API_KEY="${2:-}"; shift 2 ;;
    --model-a) MODEL_A="${2:-}"; shift 2 ;;
    --model-b) MODEL_B="${2:-}"; shift 2 ;;
    --keep-threads) KEEP_THREADS=1; shift ;;
    -h|--help) usage; exit 0 ;;
    *) echo "Unknown argument: $1" >&2; usage >&2; exit 2 ;;
  esac
done

[[ -n "$ACCOUNT_ID" ]] || { echo "--account is required" >&2; exit 2; }
[[ -n "$API_KEY" ]] || { echo "--api-key is required or set LLMGATEWAY_API_KEY" >&2; exit 2; }
BASE_URL="${BASE_URL%/}"

cleanup() {
  if [[ "$KEEP_THREADS" -eq 0 ]]; then
    for thread in "${THREADS[@]:-}"; do
      [[ -n "$thread" ]] || continue
      curl -fsS -X DELETE -H "Authorization: Bearer $API_KEY" "$BASE_URL/v1/threads/$thread" >/dev/null 2>&1 || true
    done
  else
    echo "[gemini-model-live] Keeping threads: ${THREADS[*]:-none}"
  fi
  rm -rf "$TMP_DIR"
}
trap cleanup EXIT

api() {
  local method="$1" path="$2" output="$3" body="${4:-}"
  local args=(-fsS -X "$method" -H "Authorization: Bearer $API_KEY")
  if [[ -n "$body" ]]; then
    args+=(-H "Content-Type: application/json" --data-binary "$body")
  fi
  curl "${args[@]}" "$BASE_URL$path" -o "$output"
}

echo "[gemini-model-live] Refreshing model catalog for $ACCOUNT_ID"
api POST "/_llmgateway/accounts/$ACCOUNT_ID/models/refresh" "$TMP_DIR/refresh.json"
api GET "/_llmgateway/accounts/$ACCOUNT_ID/models" "$TMP_DIR/models.json"

python3 - "$TMP_DIR/refresh.json" "$TMP_DIR/models.json" "$ACCOUNT_ID" "$MODEL_A" "$MODEL_B" "$TMP_DIR/selected.json" <<'PY'
import json, sys
refresh_path, models_path, account, requested_a, requested_b, output = sys.argv[1:]
refresh = json.load(open(refresh_path, encoding="utf-8"))
if int(refresh.get("discovered_models", 0)) <= 0:
    raise SystemExit("MODEL ACCEPTANCE FAILED: model discovery returned zero models")
payload = json.load(open(models_path, encoding="utf-8"))
models = []
for model in payload.get("data", []):
    if model.get("provider") != "gemini-web":
        continue
    bindings = model.get("accounts") or []
    if any(
        b.get("account_id") == account
        and b.get("enabled")
        and b.get("availability") == "available"
        and b.get("discovered")
        for b in bindings
    ):
        models.append(model)
if len(models) < 2:
    raise SystemExit("MODEL ACCEPTANCE FAILED: need at least two discovered Gemini models")
def resolve(requested, index):
    if requested:
        for model in models:
            if model.get("id") == requested or model.get("external_id") == requested:
                return model
        raise SystemExit(f"MODEL ACCEPTANCE FAILED: requested model {requested!r} was not discovered")
    return models[index]
a, b = resolve(requested_a, 0), resolve(requested_b, 1)
if a["id"] == b["id"]:
    raise SystemExit("MODEL ACCEPTANCE FAILED: Model A and Model B are identical")
json.dump({"a": a, "b": b}, open(output, "w", encoding="utf-8"))
print(f"[gemini-model-live] Model A: {a['display_name']} [{a['id']}]")
print(f"[gemini-model-live] Model B: {b['display_name']} [{b['id']}]")
PY

MODEL_A_ID="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["a"]["id"])' "$TMP_DIR/selected.json")"
MODEL_A_EXTERNAL="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["a"]["external_id"])' "$TMP_DIR/selected.json")"
MODEL_B_ID="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["b"]["id"])' "$TMP_DIR/selected.json")"
MODEL_B_EXTERNAL="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["b"]["external_id"])' "$TMP_DIR/selected.json")"

api GET "/_llmgateway/browser-accounts/$ACCOUNT_ID/runtime" "$TMP_DIR/runtime.json"
SESSION_ID="$(python3 -c 'import json,sys; x=json.load(open(sys.argv[1])); print(x.get("session_id",""))' "$TMP_DIR/runtime.json")"
BROWSER_RUNNING="$(python3 -c 'import json,sys; print("1" if json.load(open(sys.argv[1])).get("browser_running") else "0")' "$TMP_DIR/runtime.json")"
if [[ "$BROWSER_RUNNING" == "1" ]]; then
  api POST "/_llmgateway/browser-sessions/$SESSION_ID/driver/stop" "$TMP_DIR/stop.json"
  sleep 0.3
fi

create_thread() {
  local title="$1" model="$2" output="$3"
  local body
  body="$(python3 - "$title" "$model" <<'PY'
import json,sys
print(json.dumps({"title":sys.argv[1],"model":sys.argv[2]}))
PY
)"
  api POST "/v1/threads" "$output" "$body"
  LAST_THREAD="$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["id"])' "$output")"
  THREADS+=("$LAST_THREAD")
}

send_model() {
  local thread="$1" model="$2" external="$3" prompt="$4" prefix="$5"
  local body
  body="$(python3 - "$prompt" "$model" <<'PY'
import json,sys
print(json.dumps({"content":sys.argv[1],"model":sys.argv[2],"stream":False}))
PY
)"
  curl -fsS -D "$TMP_DIR/$prefix.headers" -o "$TMP_DIR/$prefix.json" \
    -X POST -H "Authorization: Bearer $API_KEY" -H "Content-Type: application/json" \
    --data-binary "$body" "$BASE_URL/v1/threads/$thread/messages"
  local actual expected
  actual="$(awk 'BEGIN{IGNORECASE=1} /^x-llmgateway-route:/{gsub("\r",""); sub(/^[^:]+:[[:space:]]*/,""); print; exit}' "$TMP_DIR/$prefix.headers")"
  expected="discovered:$ACCOUNT_ID:$external"
  [[ "$actual" == "$expected" ]] || { echo "MODEL ACCEPTANCE FAILED: route $actual != $expected" >&2; exit 1; }
  api GET "/_llmgateway/browser-accounts/$ACCOUNT_ID/runtime" "$TMP_DIR/$prefix.runtime.json"
  python3 - "$TMP_DIR/$prefix.json" "$TMP_DIR/$prefix.runtime.json" "$model" <<'PY'
import json,sys
body=json.load(open(sys.argv[1],encoding="utf-8"))
runtime=json.load(open(sys.argv[2],encoding="utf-8"))
model=sys.argv[3]
text=((body.get("choices") or [{}])[0].get("message") or {}).get("content")
if not str(text or "").strip():
    raise SystemExit(f"MODEL ACCEPTANCE FAILED: {model} returned empty text")
last=runtime.get("last_execution") or {}
if runtime.get("browser_running"):
    raise SystemExit(f"MODEL ACCEPTANCE FAILED: Chromium running after {model}")
if last.get("transport") != "direct-http" or last.get("browser_fallback") or last.get("adapter_id") != "gemini-web-http":
    raise SystemExit(f"MODEL ACCEPTANCE FAILED: unexpected execution telemetry {last}")
PY
}

create_thread "Gemini model A acceptance" "$MODEL_A_ID" "$TMP_DIR/thread-a.json"
THREAD_A="$LAST_THREAD"
send_model "$THREAD_A" "$MODEL_A_ID" "$MODEL_A_EXTERNAL" "Reply with exactly: model-a" "a"
api GET "/_llmgateway/threads/$THREAD_A/browser-affinity/$ACCOUNT_ID" "$TMP_DIR/affinity-a.json"

create_thread "Gemini model B acceptance" "$MODEL_B_ID" "$TMP_DIR/thread-b.json"
THREAD_B="$LAST_THREAD"
send_model "$THREAD_B" "$MODEL_B_ID" "$MODEL_B_EXTERNAL" "Reply with exactly: model-b" "b"
api GET "/_llmgateway/threads/$THREAD_B/browser-affinity/$ACCOUNT_ID" "$TMP_DIR/affinity-b.json"

python3 - "$TMP_DIR/affinity-a.json" "$TMP_DIR/affinity-b.json" <<'PY'
import json,sys
a=json.load(open(sys.argv[1],encoding="utf-8"))
b=json.load(open(sys.argv[2],encoding="utf-8"))
ua=((a.get("mapping") or {}).get("conversation_url") or "")
ub=((b.get("mapping") or {}).get("conversation_url") or "")
if not ua or not ub:
    raise SystemExit("MODEL ACCEPTANCE FAILED: native Gemini affinity missing")
if ua == ub:
    raise SystemExit("MODEL ACCEPTANCE FAILED: two local threads share one native conversation")
PY

echo
echo "GEMINI BROWSERLESS MODEL SELECTION: PASS"
echo "Account: $ACCOUNT_ID"
echo "Model A: $MODEL_A_ID -> discovered:$ACCOUNT_ID:$MODEL_A_EXTERNAL"
echo "Model B: $MODEL_B_ID -> discovered:$ACCOUNT_ID:$MODEL_B_EXTERNAL"
echo "Chromium running: false"
