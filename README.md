# llmgateway

A local-first universal LLM gateway written in Rust. Point coding agents and OpenAI/Anthropic-compatible clients at one endpoint, or use the built-in local chat UI, while llmgateway selects account/model routes, keeps conversation state, compiles long-running context, and fails over when an upstream becomes unavailable or rate-limited.

## What exists in v0.5

- OpenAI-compatible `POST /v1/chat/completions`
- OpenAI-compatible `POST /v1/responses` for clients such as Codex
- Anthropic-compatible `POST /v1/messages` for clients such as Claude Code
- OpenAI-compatible `GET /v1/models`
- Persistent server-side threads and messages in SQLite
- Stateful `previous_response_id` for Responses API chains
- Sticky route affinity per persistent thread and Responses chain
- Context Engine with token estimation and per-model budgets
- Rolling conversation-memory checkpoints
- Automatic compaction before persistent-thread requests exceed their configured context budget
- Recent messages kept verbatim while older turns are summarized
- Manual context inspection and compaction APIs
- Full original transcript retained even after compaction
- Multi-provider and multi-account configuration
- SQLite-backed model catalog and per-account model discovery
- Enable/disable a model for an individual account
- Canonical physical model IDs such as `gemini/gemini-3.7-flash`
- Dynamic routes for discovered account/model pairs
- Virtual models such as `llmgateway-auto`, `llmgateway-coding`, and `llmgateway-best`
- Ordered failover and route cooldown for 401/403/429/5xx responses
- OpenAI SSE passthrough and Anthropic SSE translation
- Embedded local chat/admin UI with no frontend build step
- Local UI backed by server-side SQLite threads
- Example configurations for Claude Code, Codex, and OpenCode

The gateway currently uses OpenAI Chat Completions as the common upstream protocol. OpenAI Responses and Anthropic Messages are translated into that shape before routing.

## Architecture

```text
Claude Code ─ Anthropic Messages ─┐
Codex ───── OpenAI Responses ─────┤
OpenCode ───── OpenAI Chat ───────┤
Local UI ───── Thread API ─────────┤
                                  ▼
                           ┌──────────────┐
                           │  llmgateway  │
                           └──────┬───────┘
                                  │
                   ┌──────────────┴──────────────┐
                   │                             │
             Conversation                   Model catalog
                store                          store
                   │                             │
          immutable transcript          account availability
                   │                             │
             Context Engine                     │
       checkpoint + recent turns                │
                   └──────────────┬──────────────┘
                                  │
                           route planning
                                  │
             ┌────────────────────┼────────────────────┐
             ▼                    ▼                    ▼
        Gemini acct A        Qwen acct A        OpenRouter
        Gemini acct B        Qwen acct B        other APIs
```

## Quick start

```bash
cp config/llmgateway.example.toml config/llmgateway.toml
cp .env.example .env
cargo run --release
```

Set `LLMGATEWAY_API_KEY` and at least one upstream credential in `.env`.

Default endpoint:

```text
http://127.0.0.1:7331
```

The default SQLite database is:

```text
data/llmgateway.db
```

## Context Engine

Persistent threads no longer send the complete transcript blindly on every turn. llmgateway estimates the input size and builds an execution context from:

```text
Conversation memory checkpoint
        +
Recent verbatim messages
        +
Current user message
```

The original messages remain untouched in `thread_messages`. A checkpoint only changes what is sent to the next model.

This means a long conversation can move from Gemini to Qwen or another route without depending on a provider-native conversation ID.

### Default context configuration

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
```

`target_tokens` is llmgateway's conservative input budget. When a selected physical model has a smaller discovered context window, llmgateway lowers the usable budget and reserves `reserve_output_tokens` for generation.

If `summary_model` is omitted, the requested/thread model is used to create checkpoints. The summarizer is routed through the same gateway and can therefore use the normal account pool and failover logic.

### Context status

```bash
curl http://127.0.0.1:7331/v1/threads/<thread_id>/context \
  -H "Authorization: Bearer $LLMGATEWAY_API_KEY"
```

Example shape:

```json
{
  "thread_id": "thread_...",
  "state": "compressed",
  "source_tokens": 41280,
  "prepared_tokens": 11840,
  "budget_tokens": 16000,
  "trigger_tokens": 13600,
  "checkpoint": {
    "through_ordinal": 46,
    "summary": "...",
    "summary_model": "llmgateway-auto",
    "route_id": "qwen-coder"
  }
}
```

States are:

- `full`: transcript currently fits without a checkpoint
- `needs_compaction`: the estimated transcript has crossed the trigger and no usable checkpoint exists yet
- `compressed`: an earlier segment is represented by a durable checkpoint

### Force a checkpoint

```bash
curl -X POST http://127.0.0.1:7331/v1/threads/<thread_id>/compact \
  -H "Authorization: Bearer $LLMGATEWAY_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{}'
```

You may optionally choose the model used for that compaction:

```json
{"model":"llmgateway-best"}
```

### Context diagnostics on thread responses

Persistent thread message responses include:

```text
x-llmgateway-context: full | compressed
x-llmgateway-context-tokens: <estimated prepared tokens>
x-llmgateway-context-budget: <budget tokens>
x-llmgateway-route: <actual route>
```

These headers are optional diagnostics. Clients do not need to understand them.

## Local UI

Open:

```text
http://127.0.0.1:7331/
```

The UI uses the persistent thread API. A brand-new chat stays as an in-browser draft until the first message is sent. At that point the gateway creates a server thread and SQLite becomes the source of truth for its history.

The UI supports persistent chat threads, streaming, virtual/physical model selection, automatic account selection, sticky routing, failover, route display, model discovery, and account/model enable-disable controls.

When upgrading from v0.3, the UI can import old browser-local chat history into SQLite when the server has no persistent threads yet.

## Persistent Thread API

Create a thread:

```bash
curl -X POST http://127.0.0.1:7331/v1/threads \
  -H "Authorization: Bearer $LLMGATEWAY_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "title":"Kafka deep dive",
    "model":"llmgateway-auto"
  }'
```

List threads:

```bash
curl http://127.0.0.1:7331/v1/threads \
  -H "Authorization: Bearer $LLMGATEWAY_API_KEY"
```

Read one thread:

```bash
curl http://127.0.0.1:7331/v1/threads/<thread_id> \
  -H "Authorization: Bearer $LLMGATEWAY_API_KEY"
```

Send a message while letting llmgateway own the context:

```bash
curl -N -X POST http://127.0.0.1:7331/v1/threads/<thread_id>/messages \
  -H "Authorization: Bearer $LLMGATEWAY_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "content":"Explain Kafka zero copy",
    "model":"llmgateway-auto",
    "stream":true
  }'
```

The request contains only the new message. llmgateway compiles prior context from SQLite, prefers the thread's sticky route, and stores the completed assistant message back into the full transcript.

Delete a thread:

```bash
curl -X DELETE http://127.0.0.1:7331/v1/threads/<thread_id> \
  -H "Authorization: Bearer $LLMGATEWAY_API_KEY"
```

`POST /v1/threads` also accepts an optional `messages` array, used by the UI to import v0.3 local history.

## Responses API state

`previous_response_id` is supported. llmgateway stores the normalized OpenAI message context plus the selected route for each completed response.

```bash
curl http://127.0.0.1:7331/v1/responses \
  -H "Authorization: Bearer $LLMGATEWAY_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "model":"llmgateway-coding",
    "input":"Inspect this design"
  }'
```

Then continue with the returned ID:

```bash
curl http://127.0.0.1:7331/v1/responses \
  -H "Authorization: Bearer $LLMGATEWAY_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "model":"llmgateway-coding",
    "previous_response_id":"resp_...",
    "input":"Now challenge the concurrency assumptions"
  }'
```

For streaming Responses requests, state is committed after the `response.completed` event is consumed.

## Stateless compatibility APIs

`/v1/chat/completions` and `/v1/messages` remain stateless by design because many external tools already own their agent loop and message history. Use `/v1/threads` when llmgateway should own long-running conversation state and compaction.

## Sticky routing

A persistent conversation stores the route that successfully served its last turn:

```text
thread_123
  model: llmgateway-auto
  sticky_route: discovered:qwen-primary:qwen-coder
```

The next turn tries that route first. If it is unavailable, rate-limited, or cooling down, the route planner continues through fallback candidates. The successful replacement route becomes the new sticky route.

The same affinity behavior is used for `previous_response_id` chains.

## Model catalog and discovery

Configured route models are seeded into SQLite at startup. They begin with `unknown` availability until provider discovery verifies them.

```bash
curl http://127.0.0.1:7331/_llmgateway/accounts \
  -H "Authorization: Bearer $LLMGATEWAY_API_KEY"
```

Refresh one account:

```bash
curl -X POST \
  http://127.0.0.1:7331/_llmgateway/accounts/gemini-primary/models/refresh \
  -H "Authorization: Bearer $LLMGATEWAY_API_KEY"
```

Enable or disable a model for one account:

```bash
curl -X PATCH \
  http://127.0.0.1:7331/_llmgateway/accounts/gemini-primary/models \
  -H "Authorization: Bearer $LLMGATEWAY_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{"model_id":"gemini/gemini-3.7-flash","enabled":false}'
```

`GET /v1/models` exposes virtual policies, canonical physical models, and older explicit route IDs retained for compatibility.

## Choose a model, not an account

```bash
curl http://127.0.0.1:7331/v1/chat/completions \
  -H "Authorization: Bearer $LLMGATEWAY_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "model":"gemini/gemini-3.7-flash",
    "messages":[{"role":"user","content":"Explain Kafka zero copy"}]
  }'
```

The `gemini/` prefix locks the provider/model choice, but llmgateway still chooses the best enabled Gemini account exposing that model.

## Claude Code

```bash
export LLMGATEWAY_API_KEY=tmx_change_me
export ANTHROPIC_BASE_URL=http://127.0.0.1:7331
export ANTHROPIC_AUTH_TOKEN="$LLMGATEWAY_API_KEY"
claude
```

## Codex

Use `examples/codex-config.toml`. The provider uses the Responses wire protocol and supports continued state through `previous_response_id`.

## OpenCode

Use `examples/opencode-config.json`. OpenCode can enumerate `/v1/models` and select virtual or discovered physical models.

## Authentication and security

Compatibility/admin/thread APIs accept either:

```text
Authorization: Bearer <LLMGATEWAY_API_KEY>
```

or:

```text
x-api-key: <LLMGATEWAY_API_KEY>
```

Provider credentials are never returned to clients. The default bind address is `127.0.0.1`. Provider keys are loaded from environment variables and active config/SQLite files are gitignored. Do not expose the service publicly without TLS, network controls, and rate limiting.

## Failover behavior

Retryable statuses currently include 401, 403, 408, 409, 429, and common 5xx failures. Failover happens before a successful upstream stream is handed to the client. Once output is already streaming, llmgateway does not splice a second model into the same answer.

## Context design guarantees

1. Compaction never deletes the original thread transcript.
2. Recent messages are kept verbatim after a checkpoint.
3. A checkpoint is model/provider independent text memory and can be handed to a different route.
4. If the prepared history must be trimmed further, the latest turn is preserved even when that slightly exceeds the configured budget.
5. A checkpoint records which model/route produced it for diagnostics, but does not depend on that route later.

## Roadmap

1. Structured checkpoint memory (`facts`, `decisions`, `open_questions`, `code_context`) and semantic retrieval.
2. Persistent quota-domain and usage tracking.
3. Browser-session accounts using isolated persistent profiles.
4. Cost, latency, capability, and context-aware route scoring.
5. Per-client API keys, budgets, and routing policies.
6. Context/route explanations and usage dashboards in the local UI.

## License

MIT
