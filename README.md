# llmgateway

A local-first universal LLM gateway written in Rust.

Point Claude Code, Codex, OpenCode, OpenAI-compatible clients, Anthropic-compatible clients, or the built-in local chat UI at one endpoint. llmgateway owns durable conversation state while routing each turn across providers, accounts, and models with failover.

> One conversation. Any model. Context intact.

## Current highlights (v0.29)

- OpenAI-compatible `POST /v1/chat/completions`
- OpenAI-compatible `POST /v1/responses`
- Anthropic-compatible `POST /v1/messages`
- OpenAI-compatible `GET /v1/models`
- Persistent server-side threads in SQLite
- Stateful `previous_response_id` chains
- Multi-provider and multi-account routing
- Browser-first virtual-model routing with optional API fallback
- Browser session startup reconciliation and automatic crash recovery
- First-class `browser-gemini` and `browser-qwen` providers
- Browser Accounts wizard for managed Gemini/Qwen account creation
- Hot activation of new browser accounts, routes, sessions, and catalog entries without gateway restart
- Browser account disable/re-enable, re-authenticate, restart, stop, and recovery controls
- Request-pinned immutable config snapshots during hot reload
- Versioned browser adapter contract with page-drift diagnostics
- Stateless per-request browser chat tabs for built-in providers
- Sticky route affinity with automatic fallback
- Model catalog and per-account model discovery
- Canonical physical model IDs plus virtual routing models
- Structured Memory IR for durable cross-model context
- Rolling context checkpoints with immutable full transcripts
- Tool-call/tool-result atomicity during context compaction
- Model-aware input budgets
- v0.7 local semantic/hybrid retrieval over checkpointed transcript history
- Retrieval inspector API and diagnostic response headers
- Embedded local UI with no frontend build step
- Docker build and end-to-end CI smoke tests

The common upstream protocol is currently OpenAI Chat Completions. Responses API and Anthropic Messages requests are normalized before routing.

## Architecture

```text
Claude Code ─ Anthropic Messages ─┐
Codex ───── OpenAI Responses ─────┤
OpenCode ───── OpenAI Chat ───────┤
Local UI ───── Persistent Threads ─┤
                                  ▼
                           ┌──────────────┐
                           │  llmgateway  │
                           └──────┬───────┘
                                  │
              ┌───────────────────┴───────────────────┐
              │                                       │
      Conversation Engine                       Model Catalog
              │                                       │
       immutable transcript                    account models
              │                                       │
       Structured Memory IR                          routes
              │                                       │
       rolling checkpoint                             │
              │                                       │
       semantic retrieval                             │
              │                                       │
          recent turns                                │
              └───────────────────┬───────────────────┘
                                  │
                            Route Planner
                                  │
                 ┌────────────────┼────────────────┐
                 ▼                ▼                ▼
            Gemini × N         Qwen × N       OpenRouter
                                             other APIs
```

The ownership rule is deliberate: **the conversation belongs to llmgateway, not to a provider-native chat ID**. Providers are execution engines for individual turns.

## Quick start

```bash
cp config/llmgateway.example.toml config/llmgateway.toml
cp .env.example .env
cargo run --release
```

Set `LLMGATEWAY_API_KEY`. API-backed accounts also need their configured upstream credentials; first-class browser accounts do not require dummy API keys.

Default endpoint:

```text
http://127.0.0.1:7331
```

Default database:

```text
data/llmgateway.db
```

Open the local UI at:

```text
http://127.0.0.1:7331/
```

## Built-in browser providers

v0.28 includes first-class Gemini Web and Qwen Web providers:

```toml
[[providers]]
id = "gemini-web"
kind = "browser-gemini"

[[providers]]
id = "qwen-web"
kind = "browser-qwen"
```

Bind each account to an isolated browser session, enable the corresponding Chromium session, sign in normally, and let the adapter probe decide whether the provider page is compatible before Router makes it eligible. No dummy upstream API key is required.

In v0.29, the normal setup path is the **Accounts** UI: choose **Add browser account**, select Gemini Web or Qwen Web, and llmgateway safely writes the linked session/binding/Chromium/provider/account/route configuration and hot-activates it. The gateway process does not need to restart. From the same account card you can disable/re-enable routing, re-authenticate, restart Chromium, stop the browser, and inspect lifecycle/adapter state. Isolated Chromium profiles are preserved when an account is disabled or stopped.

See [Browser Accounts UX](docs/browser-accounts-ux.md) for the managed setup lifecycle and admin endpoints.

Built-in adapters default to fresh provider chat tabs per request while reusing the authenticated Chromium profile. Optional `model_labels` select provider UI models explicitly; without a mapping, the current provider UI model is preserved.

See [Browser provider adapters](docs/browser-provider-adapters.md) for complete Gemini/Qwen configuration, adapter diagnostics, page-drift recovery, and the coding-agent tool bridge.

## Gateway APIs

```text
POST /v1/chat/completions
POST /v1/responses
POST /v1/messages
GET  /v1/models
```

Thread APIs:

```text
POST   /v1/threads
GET    /v1/threads
GET    /v1/threads/{thread_id}
DELETE /v1/threads/{thread_id}
POST   /v1/threads/{thread_id}/messages
GET    /v1/threads/{thread_id}/context
GET    /v1/threads/{thread_id}/memory
POST   /v1/threads/{thread_id}/compact
POST   /v1/threads/{thread_id}/retrieve
```

Admin/model APIs:

```text
GET   /_llmgateway/health
GET   /_llmgateway/models
GET   /_llmgateway/accounts
GET   /_llmgateway/accounts/{account_id}/models
PATCH /_llmgateway/accounts/{account_id}/models
POST  /_llmgateway/accounts/{account_id}/models/refresh
```

## Persistent threads

Create a thread:

```bash
curl -X POST http://127.0.0.1:7331/v1/threads \
  -H "Authorization: Bearer $LLMGATEWAY_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{"title":"Kafka deep dive","model":"llmgateway-auto"}'
```

Send only the new turn:

```bash
curl -N -X POST http://127.0.0.1:7331/v1/threads/<thread_id>/messages \
  -H "Authorization: Bearer $LLMGATEWAY_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{"content":"Explain Kafka zero copy","stream":true}'
```

llmgateway loads and compiles prior context from SQLite, routes the request, and appends the completed assistant message to the immutable transcript.

## Context pipeline

Long-running persistent threads use four context layers:

```text
Structured Memory IR
        +
Relevant retrieved historical excerpts
        +
Recent verbatim turns
        +
Current user turn
```

Older turns are never deleted. A checkpoint only changes what is sent to the next model.

### Structured Memory IR

The current durable memory schema is:

```json
{
  "facts": [],
  "decisions": [],
  "constraints": [],
  "user_preferences": [],
  "entities": [],
  "code_context": [],
  "open_questions": [],
  "rolling_summary": ""
}
```

Memory snapshots are stored separately from the full transcript and include schema version, `through_ordinal`, model, route provenance, and update time.

Inspect memory:

```bash
curl http://127.0.0.1:7331/v1/threads/<thread_id>/memory \
  -H "Authorization: Bearer $LLMGATEWAY_API_KEY"
```

### Semantic context retrieval

v0.7 retrieves exact historical details from the part of the transcript already represented by a checkpoint. It uses a local hybrid scorer with:

- query-term overlap
- within-thread rarity/IDF weighting
- repeated-term weighting
- bigram/phrase overlap
- query coverage
- a small recency tie-breaker

There is no embedding API requirement and therefore no extra network call or retrieval billing.

Retrieval only consumes **spare** context budget after durable memory, recent messages, and the current turn are fitted. An augmented context is accepted only when it remains inside the model-aware budget.

Default configuration:

```toml
[context]
enabled = true
target_tokens = 16000
reserve_output_tokens = 4000
recent_messages = 12
compaction_trigger_ratio = 0.85
summary_input_tokens = 12000
summary_max_tokens = 1200
# summary_model = "llmgateway-auto"

retrieval_enabled = true
retrieval_max_chunks = 3
retrieval_max_tokens = 2400
retrieval_min_score = 0.35
```

Inspect retrieval without calling an LLM:

```bash
curl -X POST http://127.0.0.1:7331/v1/threads/<thread_id>/retrieve \
  -H "Authorization: Bearer $LLMGATEWAY_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{"query":"How did we handle optimistic locking conflicts?"}'
```

See [`docs/semantic-retrieval.md`](docs/semantic-retrieval.md) for scoring, budget rules, diagnostics, and limitations.

### Context diagnostics

Persistent-thread responses may include:

```text
x-llmgateway-context: full | compressed
x-llmgateway-context-source-tokens: <estimated source tokens>
x-llmgateway-context-tokens: <prepared tokens>
x-llmgateway-context-budget: <budget tokens>
x-llmgateway-context-checkpoint: <checkpoint id>
x-llmgateway-retrieved-chunks: <count>
x-llmgateway-route: <actual route>
```

Clients do not need to understand these headers.

## Model and account routing

`GET /v1/models` exposes:

- virtual policies such as `llmgateway-auto`, `llmgateway-coding`, and `llmgateway-best`
- canonical physical models such as `gemini/gemini-3.7-flash`
- explicit route IDs retained for compatibility

A physical model can be available through multiple accounts/routes. The user selects the model; llmgateway selects the account.

Example:

```text
Gemini model
   │
   ├─ account A   healthy
   ├─ account B   rate limited
   └─ OpenRouter  paid fallback
   │
   ▼
best eligible route
```

When a sticky route fails with a retryable condition, fallback continues through other eligible routes and the successful route becomes the new affinity. Task-aware routing can override an old sticky route when a materially better task fit exists, and v0.27 also prevents an old API affinity from jumping ahead of the configured browser-first transport policy.

v0.27 makes virtual-model execution browser-first by default. Eligible browser routes are ranked before API routes; the existing priority, quota, adaptive latency/reliability, and task-fit score still chooses the best route inside each transport tier. Set `routing.execution_preference = "balanced"` to remove the transport preference, or `"api-first"` to invert it. `routing.api_fallback = false` disables API fallback for browser-first virtual models.

Browser sessions are reconciled with the live Chromium/CDP runtime at startup and periodically afterward. A still-running browser is reconnected after a gateway restart. A previously-ready browser that crashes can be relaunched with the same isolated profile and re-verified automatically; a deliberate Stop, login-required state, or attention state is not auto-launched.

v0.28 adds built-in Gemini Web and Qwen Web adapters behind a versioned contract. Each adapter is probed before it becomes routable. Missing/changed provider UI controls are surfaced as `browser_adapter_incompatible` rather than generic transport failures. Built-in requests default to a fresh provider chat tab inside the same authenticated profile, so provider-native conversation history does not silently compete with llmgateway's own persistent context. Optional `model_labels` map logical route models to provider UI model names, while selector overrides provide a local escape hatch when a provider changes DOM details.

v0.26 added task-aware routing on top of readiness, quota, configured priority, and adaptive latency/reliability. Requests are classified locally as coding, reasoning, long-context, simple chat, or general. Routes can advertise policy metadata such as `coding`, `reasoning`, `long-context`, `cheap`, and `fast`; unknown metadata remains neutral for backward compatibility. A known `context_window` that cannot fit the request is excluded before ranking.

The route score remains explainable:

```text
final_score = base_priority + quota_penalty + adaptive_penalty + task_adjustment
```

Lower scores win. See [`docs/task-aware-routing.md`](docs/task-aware-routing.md) for classifier signals, capability tags, explicit task hints, and explain examples.

## Claude Code

```bash
export LLMGATEWAY_API_KEY=tmx_change_me
export ANTHROPIC_BASE_URL=http://127.0.0.1:7331
export ANTHROPIC_AUTH_TOKEN="$LLMGATEWAY_API_KEY"
claude
```

Model aliases can map Claude model names to virtual routing policies.

## Codex

Use [`examples/codex-config.toml`](examples/codex-config.toml). Codex uses the Responses wire protocol. `previous_response_id` chains are persisted by llmgateway.

## OpenCode

Use [`examples/opencode-config.json`](examples/opencode-config.json). OpenCode can enumerate `/v1/models` and select virtual or discovered physical models.

## Authentication and security

Gateway/admin/thread APIs accept either:

```text
Authorization: Bearer <LLMGATEWAY_API_KEY>
```

or:

```text
x-api-key: <LLMGATEWAY_API_KEY>
```

Provider credentials are never returned to clients. The default bind address is `127.0.0.1`. Active config, provider keys, and SQLite data are gitignored.

Do not expose the service publicly without TLS, network controls, authentication, and rate limiting.

## Context guarantees

1. Compaction never deletes or rewrites the original transcript.
2. Recent messages remain verbatim after a checkpoint.
3. The current user turn is part of budget fitting and remains highest priority.
4. Tool calls and their tool results remain atomic during compaction/trimming.
5. Structured memory is provider independent.
6. Retrieval searches only the checkpointed historical region, so recent messages are not duplicated.
7. Retrieval is accepted only when the final prepared context remains within budget.
8. A recent explicit correction outranks older retrieved history.
9. Memory/checkpoints record model and route provenance but do not depend on that route later.

## Current retrieval limitation

v0.7 uses a local lexical/hybrid scorer rather than vector embeddings. It is intentionally excellent at concrete technical referents such as symbols, APIs, error codes, model names, filenames, and architecture terms while remaining zero-config and cheap.

A future retriever can add local/remote embeddings and reranking behind the same context-layer contract.

## Roadmap

The roadmap is now **browser-first** because authenticated browser accounts are the primary local execution path. API-key accounts remain supported as optional fallbacks.

Current browser milestones:

- **v0.27 Browser Account Reliability** ✅ - startup reconciliation, reconnect/recovery, explicit lifecycle states, live CDP readiness, browser-first preference, and failover/recovery E2E.
- **v0.28 Production-grade Browser Provider Adapters** ✅ - first-class Gemini/Qwen providers, contract v1, adapter health/page-drift diagnostics, model mapping, stateless provider tabs, and deterministic fake-page/CDP fixtures.
- **v0.29 Browser Accounts UX** ✅ - managed Gemini/Qwen account wizard, safe config persistence, hot activation, lifecycle controls, immutable request snapshots, and deterministic hot-activation E2E coverage.

Next milestones:

1. **v0.30 True Browser Streaming and Cancellation** - incremental CDP streaming, cancellation, backpressure, disconnect handling, and streaming compatibility tests.
2. **v0.31 Browser-aware Routing Intelligence** - multi-account fairness, session-aware affinity, browser-specific recovery, and deterministic browser-to-API fallback.
3. **v0.32+** - per-client policies, usage/cost intelligence, model/cost intelligence, production hardening, and distribution.

See the detailed [browser-first roadmap](docs/roadmap.md), including release gates for v1.0.

## License

MIT
