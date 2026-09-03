# llmgateway

A local-first universal LLM gateway written in Rust. Point coding agents and OpenAI/Anthropic-compatible clients at one endpoint, or use the built-in local chat UI, while llmgateway selects account/model routes, keeps durable conversation state, compiles long-running context, and fails over when an upstream becomes unavailable or rate-limited.

## What exists in v0.6

- OpenAI-compatible `POST /v1/chat/completions`
- OpenAI-compatible `POST /v1/responses` for clients such as Codex
- Anthropic-compatible `POST /v1/messages` for clients such as Claude Code
- OpenAI-compatible `GET /v1/models`
- Persistent server-side threads and messages in SQLite
- Stateful `previous_response_id` for Responses API chains
- Sticky route affinity per persistent thread and Responses chain
- Context Engine with token estimation and per-model budgets
- Structured Memory IR with schema-versioned SQLite snapshots
- Memory sections for facts, decisions, constraints, user preferences, entities, code context, open questions, and a rolling summary
- Rolling checkpoints rendered from structured memory
- Automatic compaction before persistent-thread requests exceed their configured context budget
- Recent messages kept verbatim while older turns are represented by memory
- Tool-call/tool-result exchanges kept atomic during compaction and trimming
- Current user turns included in context-budget fitting
- Manual context inspection and compaction APIs
- Dedicated structured-memory inspection API
- Full original transcript retained even after repeated compaction
- Multi-provider and multi-account configuration
- SQLite-backed model catalog and per-account model discovery
- Enable/disable a model for an individual account
- Canonical physical model IDs such as `gemini/gemini-3.7-flash`
- Dynamic routes for discovered account/model pairs
- Virtual models such as `llmgateway-auto`, `llmgateway-coding`, and `llmgateway-best`
- Ordered failover and route cooldown for 401/403/429/5xx responses
- OpenAI SSE passthrough and Anthropic SSE translation
- Embedded local chat/admin UI with no frontend build step
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
                   │                             │
          Structured Memory IR                  │
   facts / decisions / constraints              │
 preferences / entities / code / TODOs          │
                   │                             │
          rendered checkpoint                   │
              + recent turns                    │
                   └──────────────┬──────────────┘
                                  │
                           route planning
                                  │
             ┌────────────────────┼────────────────────┐
             ▼                    ▼                    ▼
        Gemini acct A        Qwen acct A        OpenRouter
        Gemini acct B        Qwen acct B        other APIs
```

The important ownership rule is simple: the conversation belongs to llmgateway, not to a provider-native chat ID. Models are execution engines for turns; the durable thread and memory remain stable when routes change.

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

Persistent threads do not send the complete transcript blindly on every turn. llmgateway estimates the input size and builds execution context from:

```text
Structured memory checkpoint
        +
Recent verbatim messages
        +
Current user message
```

The original messages remain untouched in `thread_messages`. Compaction changes only what is sent to the next model.

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

If `summary_model` is omitted, the requested/thread model creates memory checkpoints. The memory compiler is routed through the same gateway and therefore benefits from the normal account pool, sticky-route affinity, and failover behavior.

## Structured Memory IR

v0.6 stores a schema-versioned memory snapshot separately from the immutable transcript and checkpoint history. The current schema is:

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

The model is asked to update the full structured snapshot when a checkpoint advances. llmgateway normalizes and deduplicates list items before persisting the snapshot to `thread_memories`.

If a provider returns malformed/non-JSON memory output, compaction does not discard the previous structured fields. llmgateway keeps the existing snapshot and falls back to using the returned text as the rolling summary.

Each structured snapshot records:

```text
thread_id
through_ordinal
schema_version
memory JSON
model
route_id
updated_at
```

The rendered checkpoint remains plain provider-independent text so older code paths and arbitrary upstream models do not need to understand the internal JSON schema.

### Inspect structured memory

```bash
curl http://127.0.0.1:7331/v1/threads/<thread_id>/memory \
  -H "Authorization: Bearer $LLMGATEWAY_API_KEY"
```

Example:

```json
{
  "thread_id": "thread_...",
  "memory": {
    "through_ordinal": 46,
    "schema_version": 1,
    "model": "llmgateway-auto",
    "route_id": "qwen-coder",
    "memory": {
      "facts": ["The gateway is implemented in Rust"],
      "decisions": ["Keep SQLite as the local source of truth"],
      "constraints": ["Never delete the full transcript during compaction"],
      "user_preferences": ["Prefer local-first operation"],
      "entities": ["llmgateway", "ContextEngine"],
      "code_context": ["thread_memories stores schema-versioned JSON"],
      "open_questions": ["Which retrieval strategy should v0.7 use?"],
      "rolling_summary": "The project is building provider-independent durable context."
    }
  }
}
```

Before the first checkpoint, `memory` is `null`.

## Context status

```bash
curl http://127.0.0.1:7331/v1/threads/<thread_id>/context \
  -H "Authorization: Bearer $LLMGATEWAY_API_KEY"
```

The response includes source/prepared token estimates, the active checkpoint, and the current structured-memory snapshot.

States are:

- `full`: transcript currently fits without a checkpoint
- `needs_compaction`: the estimated transcript has crossed the trigger and no usable checkpoint exists yet
- `compressed`: an earlier segment is represented by durable memory/checkpoint state

### Force a checkpoint

```bash
curl -X POST http://127.0.0.1:7331/v1/threads/<thread_id>/compact \
  -H "Authorization: Bearer $LLMGATEWAY_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{}'
```

You may optionally choose the model used for compaction:

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

Send a message while letting llmgateway own context:

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

`POST /v1/threads` also accepts an optional `messages` array, used by the UI to import older browser-local history.

## Local UI

Open:

```text
http://127.0.0.1:7331/
```

The UI uses the persistent thread API. A brand-new chat stays as an in-browser draft until the first message is sent. At that point the gateway creates a server thread and SQLite becomes the source of truth for its history.

The UI supports persistent chat threads, streaming, virtual/physical model selection, automatic account selection, sticky routing, failover, route display, model discovery, and account/model enable-disable controls.

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

1. Compaction never deletes or rewrites the original thread transcript.
2. Recent messages remain verbatim after a checkpoint.
3. Durable memory is provider independent and can be handed to a different route on the next turn.
4. Tool calls and their tool results are treated as atomic context groups so compaction/trimming does not create orphan tool results.
5. The current user turn participates in context-budget fitting and is preserved as the latest atomic group.
6. Memory snapshots and checkpoints record the model/route that produced them for diagnostics, but do not depend on that route later.
7. Repeated compaction advances `through_ordinal` while the immutable transcript remains complete.

## Roadmap

1. Semantic retrieval over structured memory and older transcript segments.
2. Memory provenance, confidence, pinning, and deterministic conflict handling.
3. Persistent quota-domain and usage tracking.
4. Browser-session accounts using isolated persistent profiles.
5. Cost, latency, capability, quota, and context-aware route scoring.
6. Per-client API keys, budgets, and routing policies.
7. Context/memory/route explanations and usage dashboards in the local UI.

## License

MIT
