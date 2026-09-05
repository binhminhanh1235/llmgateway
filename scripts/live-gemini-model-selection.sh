#!/usr/bin/env bash
set -euo pipefail

BASE_URL="${LLMGATEWAY_BASE_URL:-http://127.0.0.1:7331}"
API_KEY="${LLMGATEWAY_API_KEY:-}"
ACCOUNT_ID=""
MODEL_A=""
MODEL_B=""
KEEP_THREADS=0
SKIP_REFRESH=0
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
  --skip-refresh       Use persisted discovered models; intended for post-restart rehydration acceptance
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
    --skip-refresh) SKIP_REFRESH=1; shift ;;
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

api GET "/_llmgateway/browser-accounts/$ACCOUNT_ID/runtime" "$TMP_DIR/preflight.json"
SESSION_ID="$(python3 -c 'import json,sys; x=json.load(open(sys.argv[1])); print(x.get("session_id",""))' "$TMP_DIR/preflight.json")"
BROWSER_RUNNING="$(python3 -c 'import json,sys; print("1" if json.load(open(sys.argv[1])).get("browser_running") else "0")' "$TMP_DIR/preflight.json")"
if [[ "$BROWSER_RUNNING" == "1" ]]; then
  api POST "/_llmgateway/browser-sessions/$SESSION_ID/driver/stop" "$TMP_DIR/stop-before-refresh.json"
  sleep 0.3
fi
api GET "/_llmgateway/browser-accounts/$ACCOUNT_ID/runtime" "$TMP_DIR/preflight.json"
python3 - "$TMP_DIR/preflight.json" <<'PY'
import json,sys
runtime=json.load(open(sys.argv[1],encoding="utf-8"))
if runtime.get("browser_running"):
    raise SystemExit("MODEL ACCEPTANCE FAILED: Chromium could not be stopped before model refresh")
if not runtime.get("direct_ready") or runtime.get("effective_transport") != "direct-http":
    raise SystemExit(f"MODEL ACCEPTANCE FAILED: account is not direct-ready before model refresh: {runtime}")
PY

if [[ "$SKIP_REFRESH" -eq 1 ]]; then
  echo "[gemini-model-live] Using persisted model catalog after restart; explicit refresh is disabled"
  printf '{"skipped":true}\n' > "$TMP_DIR/refresh.json"
else
  echo "[gemini-model-live] Refreshing model catalog for $ACCOUNT_ID with Chromium stopped"
  api POST "/_llmgateway/accounts/$ACCOUNT_ID/models/refresh" "$TMP_DIR/refresh.json"
fi
api GET "/_llmgateway/accounts/$ACCOUNT_ID/models" "$TMP_DIR/models.json"

python3 - "$TMP_DIR/refresh.json" "$TMP_DIR/models.json" "$ACCOUNT_ID" "$MODEL_A" "$MODEL_B" "$SKIP_REFRESH" "$TMP_DIR/selected.json" <<'PY'
import json, sys
refresh_path, models_path, account, requested_a, requested_b, skip_refresh, output = sys.argv[1:]
refresh = json.load(open(refresh_path, encoding="utf-8"))
if skip_refresh != "1" and int(refresh.get("discovered_models", 0)) <= 0:
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

api GET "/v1/models" "$TMP_DIR/public-models.json"
python3 - "$TMP_DIR/public-models.json" "$MODEL_A_ID" "$MODEL_B_ID" <<'PY'
import json,sys
payload=json.load(open(sys.argv[1],encoding="utf-8"))
ids={str(model.get("id") or "") for model in payload.get("data",[])}
for model in sys.argv[2:]:
    if model not in ids:
        raise SystemExit(f"MODEL ACCEPTANCE FAILED: /v1/models does not expose {model!r}")
PY

api GET "/_llmgateway/browser-accounts/$ACCOUNT_ID/runtime" "$TMP_DIR/runtime.json"
python3 - "$TMP_DIR/runtime.json" <<'PY'
import json,sys
runtime=json.load(open(sys.argv[1],encoding="utf-8"))
catalog=runtime.get("model_catalog") or {}
if int(catalog.get("count") or 0) < 2:
    raise SystemExit(f"MODEL ACCEPTANCE FAILED: runtime model catalog is incomplete: {catalog}")
if catalog.get("refresh_required"):
    raise SystemExit(f"MODEL ACCEPTANCE FAILED: runtime model catalog still requires refresh: {catalog}")
if not catalog.get("discovered_at"):
    raise SystemExit(f"MODEL ACCEPTANCE FAILED: runtime model catalog has no discovery timestamp: {catalog}")
PY
python3 - "$TMP_DIR/runtime.json" <<'PY'
import json,sys
runtime=json.load(open(sys.argv[1],encoding="utf-8"))
if runtime.get("browser_running"):
    raise SystemExit("MODEL ACCEPTANCE FAILED: Chromium started during browserless model refresh")
PY

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
  python3 - "$TMP_DIR/$prefix.json" "$TMP_DIR/$prefix.runtime.json" "$model" "$external" <<'PY'
import json,sys
body=json.load(open(sys.argv[1],encoding="utf-8"))
runtime=json.load(open(sys.argv[2],encoding="utf-8"))
model=sys.argv[3]
external=sys.argv[4]
text=((body.get("choices") or [{}])[0].get("message") or {}).get("content")
if not str(text or "").strip():
    raise SystemExit(f"MODEL ACCEPTANCE FAILED: {model} returned empty text")
last=runtime.get("last_execution") or {}
if runtime.get("browser_running"):
    raise SystemExit(f"MODEL ACCEPTANCE FAILED: Chromium running after {model}")
if last.get("transport") != "direct-http" or last.get("browser_fallback") or last.get("adapter_id") != "gemini-web-http":
    raise SystemExit(f"MODEL ACCEPTANCE FAILED: unexpected execution telemetry {last}")
if last.get("model") != external:
    raise SystemExit(f"MODEL ACCEPTANCE FAILED: selected {model} executed as {last.get('model')!r}, expected {external!r}")
PY
}

stream_model() {
  local thread="$1" model="$2" external="$3" prompt="$4" prefix_name="$5"
  local body
  body="$(python3 - "$prompt" "$model" <<'PY'
import json,sys
print(json.dumps({"content":sys.argv[1],"model":sys.argv[2],"stream":True}))
PY
)"
  curl -fsS --no-buffer -D "$TMP_DIR/$prefix_name.headers" -o "$TMP_DIR/$prefix_name.sse" \
    -X POST -H "Authorization: Bearer $API_KEY" -H "Content-Type: application/json" \
    --data-binary "$body" "$BASE_URL/v1/threads/$thread/messages"

  local actual expected frames
  actual="$(awk 'BEGIN{IGNORECASE=1} /^x-llmgateway-route:/{gsub("\r",""); sub(/^[^:]+:[[:space:]]*/,""); print; exit}' "$TMP_DIR/$prefix_name.headers")"
  expected="discovered:$ACCOUNT_ID:$external"
  [[ "$actual" == "$expected" ]] || { echo "MODEL ACCEPTANCE FAILED: stream route $actual != $expected" >&2; exit 1; }

  grep -Eq '^data:[[:space:]]*\[DONE\][[:space:]]*$' "$TMP_DIR/$prefix_name.sse" \
    || { echo "MODEL ACCEPTANCE FAILED: $model stream ended without [DONE]" >&2; exit 1; }
  grep -Fq '"content"' "$TMP_DIR/$prefix_name.sse" \
    || { echo "MODEL ACCEPTANCE FAILED: $model stream returned no assistant delta" >&2; exit 1; }
  grep -Eq '"finish_reason"[[:space:]]*:[[:space:]]*"[^"]+"' "$TMP_DIR/$prefix_name.sse" \
    || { echo "MODEL ACCEPTANCE FAILED: $model stream returned no terminal finish_reason" >&2; exit 1; }
  frames="$(grep -Ec '^data:[[:space:]]*\{' "$TMP_DIR/$prefix_name.sse" || true)"
  [[ "$frames" -ge 2 ]] || { echo "MODEL ACCEPTANCE FAILED: $model stream did not expose incremental SSE frames" >&2; exit 1; }

  api GET "/_llmgateway/browser-accounts/$ACCOUNT_ID/runtime" "$TMP_DIR/$prefix_name.runtime.json"
  python3 - "$TMP_DIR/$prefix_name.runtime.json" "$model" "$external" <<'PY'
import json,sys
runtime=json.load(open(sys.argv[1],encoding="utf-8"))
model,external=sys.argv[2:4]
last=runtime.get("last_execution") or {}
if runtime.get("browser_running"):
    raise SystemExit(f"MODEL ACCEPTANCE FAILED: Chromium running after streaming {model}")
if last.get("transport") != "direct-http" or last.get("browser_fallback") or last.get("adapter_id") != "gemini-web-http":
    raise SystemExit(f"MODEL ACCEPTANCE FAILED: unexpected streaming execution telemetry {last}")
if last.get("model") != external:
    raise SystemExit(f"MODEL ACCEPTANCE FAILED: streaming {model} executed as {last.get('model')!r}, expected {external!r}")
PY

  api GET "/v1/threads/$thread" "$TMP_DIR/$prefix_name.thread.json"
  python3 - "$TMP_DIR/$prefix_name.thread.json" <<'PY'
import json,sys
thread=json.load(open(sys.argv[1],encoding="utf-8"))
for message in thread.get("messages",[]):
    if message.get("role") == "assistant" and not str(message.get("content") or "").strip():
        raise SystemExit("MODEL ACCEPTANCE FAILED: streaming persisted an empty assistant message")
PY
}

affinity_values() {
  local input="$1"
  python3 - "$input" <<'PY'
import json,sys
mapping=(json.load(open(sys.argv[1],encoding="utf-8")).get("mapping") or {})
print(mapping.get("conversation_url") or "")
print(int(mapping.get("last_synced_ordinal") or 0))
PY
}

assert_model_switch_rejected() {
  local thread="$1" from_model="$2" to_model="$3" expected_url="$4" expected_ordinal="$5"
  api GET "/v1/threads/$thread" "$TMP_DIR/model-switch-thread-before.json"
  local before_count body http_code
  before_count="$(python3 -c 'import json,sys; print(len(json.load(open(sys.argv[1],encoding="utf-8")).get("messages",[])))' "$TMP_DIR/model-switch-thread-before.json")"
  body="$(python3 - "$to_model" <<'PY'
import json,sys
print(json.dumps({
    "content":"This request must be rejected before provider submit.",
    "model":sys.argv[1],
    "stream":False
}))
PY
)"
  http_code="$(curl -sS -o "$TMP_DIR/model-switch-error.json" -w '%{http_code}' \
    -H "Authorization: Bearer $API_KEY" \
    -H "Content-Type: application/json" \
    --data-binary "$body" \
    "$BASE_URL/v1/threads/$thread/messages")"
  [[ "$http_code" -ge 400 ]] || { echo "MODEL ACCEPTANCE FAILED: switching $from_model -> $to_model unexpectedly succeeded" >&2; exit 1; }
  grep -Fq 'start a new llmgateway thread' "$TMP_DIR/model-switch-error.json" \
    || { echo "MODEL ACCEPTANCE FAILED: model-switch rejection did not come from Gemini native model affinity" >&2; cat "$TMP_DIR/model-switch-error.json" >&2; exit 1; }

  api GET "/_llmgateway/browser-accounts/$ACCOUNT_ID/runtime" "$TMP_DIR/model-switch-runtime.json"
  python3 - "$TMP_DIR/model-switch-runtime.json" <<'PY'
import json,sys
runtime=json.load(open(sys.argv[1],encoding="utf-8"))
if runtime.get("browser_running"):
    raise SystemExit("MODEL ACCEPTANCE FAILED: Chromium started while rejecting a model switch")
PY

  api GET "/_llmgateway/threads/$thread/browser-affinity/$ACCOUNT_ID" "$TMP_DIR/model-switch-affinity.json"
  python3 - "$TMP_DIR/model-switch-affinity.json" "$expected_url" "$expected_ordinal" <<'PY'
import json,sys
mapping=(json.load(open(sys.argv[1],encoding="utf-8")).get("mapping") or {})
if str(mapping.get("conversation_url") or "") != sys.argv[2]:
    raise SystemExit("MODEL ACCEPTANCE FAILED: model-switch rejection changed native Gemini conversation")
if int(mapping.get("last_synced_ordinal") or 0) != int(sys.argv[3]):
    raise SystemExit("MODEL ACCEPTANCE FAILED: model-switch rejection advanced native Gemini affinity")
PY

  api GET "/v1/threads/$thread" "$TMP_DIR/model-switch-thread-after.json"
  python3 - "$TMP_DIR/model-switch-thread-after.json" "$before_count" <<'PY'
import json,sys
count=len(json.load(open(sys.argv[1],encoding="utf-8")).get("messages",[]))
if count != int(sys.argv[2]):
    raise SystemExit("MODEL ACCEPTANCE FAILED: model-switch rejection persisted a local user/assistant message")
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

mapfile -t A1 < <(affinity_values "$TMP_DIR/affinity-a.json")
mapfile -t B1 < <(affinity_values "$TMP_DIR/affinity-b.json")
URL_A="${A1[0]:-}"
ORD_A="${A1[1]:-0}"
URL_B="${B1[0]:-}"
ORD_B="${B1[1]:-0}"
[[ -n "$URL_A" && -n "$URL_B" ]] || { echo "MODEL ACCEPTANCE FAILED: native Gemini affinity missing" >&2; exit 1; }
[[ "$URL_A" != "$URL_B" ]] || { echo "MODEL ACCEPTANCE FAILED: two local threads share one native conversation" >&2; exit 1; }

echo "[gemini-model-live] Verifying mid-thread model switch is rejected before submit"
assert_model_switch_rejected "$THREAD_A" "$MODEL_A_ID" "$MODEL_B_ID" "$URL_A" "$ORD_A"

send_model "$THREAD_A" "$MODEL_A_ID" "$MODEL_A_EXTERNAL" "Reply with exactly: model-a-continued" "a2"
api GET "/_llmgateway/threads/$THREAD_A/browser-affinity/$ACCOUNT_ID" "$TMP_DIR/affinity-a2.json"
mapfile -t A2 < <(affinity_values "$TMP_DIR/affinity-a2.json")
[[ "${A2[0]:-}" == "$URL_A" ]] || { echo "MODEL ACCEPTANCE FAILED: Model A continuation changed native Gemini conversation" >&2; exit 1; }
[[ "${A2[1]:-0}" -gt "$ORD_A" ]] || { echo "MODEL ACCEPTANCE FAILED: Model A continuation did not advance native affinity" >&2; exit 1; }

send_model "$THREAD_B" "$MODEL_B_ID" "$MODEL_B_EXTERNAL" "Reply with exactly: model-b-continued" "b2"
api GET "/_llmgateway/threads/$THREAD_B/browser-affinity/$ACCOUNT_ID" "$TMP_DIR/affinity-b2.json"
mapfile -t B2 < <(affinity_values "$TMP_DIR/affinity-b2.json")
[[ "${B2[0]:-}" == "$URL_B" ]] || { echo "MODEL ACCEPTANCE FAILED: Model B continuation changed native Gemini conversation" >&2; exit 1; }
[[ "${B2[1]:-0}" -gt "$ORD_B" ]] || { echo "MODEL ACCEPTANCE FAILED: Model B continuation did not advance native affinity" >&2; exit 1; }

stream_model "$THREAD_A" "$MODEL_A_ID" "$MODEL_A_EXTERNAL" "Reply with exactly: model-a-stream" "a-stream"
api GET "/_llmgateway/threads/$THREAD_A/browser-affinity/$ACCOUNT_ID" "$TMP_DIR/affinity-a3.json"
mapfile -t A3 < <(affinity_values "$TMP_DIR/affinity-a3.json")
[[ "${A3[0]:-}" == "$URL_A" ]] || { echo "MODEL ACCEPTANCE FAILED: Model A streaming changed native Gemini conversation" >&2; exit 1; }
[[ "${A3[1]:-0}" -gt "${A2[1]:-0}" ]] || { echo "MODEL ACCEPTANCE FAILED: Model A streaming did not advance native affinity" >&2; exit 1; }

stream_model "$THREAD_B" "$MODEL_B_ID" "$MODEL_B_EXTERNAL" "Reply with exactly: model-b-stream" "b-stream"
api GET "/_llmgateway/threads/$THREAD_B/browser-affinity/$ACCOUNT_ID" "$TMP_DIR/affinity-b3.json"
mapfile -t B3 < <(affinity_values "$TMP_DIR/affinity-b3.json")
[[ "${B3[0]:-}" == "$URL_B" ]] || { echo "MODEL ACCEPTANCE FAILED: Model B streaming changed native Gemini conversation" >&2; exit 1; }
[[ "${B3[1]:-0}" -gt "${B2[1]:-0}" ]] || { echo "MODEL ACCEPTANCE FAILED: Model B streaming did not advance native affinity" >&2; exit 1; }

echo
echo "GEMINI BROWSERLESS MODEL SELECTION: PASS"
echo "Account: $ACCOUNT_ID"
echo "Model A: $MODEL_A_ID -> discovered:$ACCOUNT_ID:$MODEL_A_EXTERNAL"
echo "Model B: $MODEL_B_ID -> discovered:$ACCOUNT_ID:$MODEL_B_EXTERNAL"
if [[ "$SKIP_REFRESH" -eq 1 ]]; then
  echo "Catalog mode: persisted-after-restart"
else
  echo "Catalog mode: live-refresh"
fi
echo "Chromium running: false"
