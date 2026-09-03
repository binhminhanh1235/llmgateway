# llmgateway

A local-first universal LLM gateway written in Rust. Point coding agents and OpenAI/Anthropic-compatible clients at one endpoint, or use the built-in local chat UI, while llmgateway selects an account/model route, keeps conversation state, and fails over when an upstream becomes unavailable or rate-limited.

## What exists in v0.4

- OpenAI-compatible `POST /v1/chat/completions`
- OpenAI-compatible `POST /v1/responses` for clients such as Codex
- Anthropic-compatible `POST /v1/messages` for clients such as Claude Code
- OpenAI-compatible `GET /v1/models`
- Persistent server-side threads and messages in SQLite
- Stateful `previous_response_id` for Responses API chains
- Sticky route affinity per persistent thread and Responses chain
- Multi-provider and multi-account configuration
- SQLite-backed model catalog
- Per-account model discovery through provider `/models` endpoints
- Per-account availability: `available`, `unknown`, `unavailable`
- Enable/disable a model for an individual account
- Canonical physical model IDs such as `gemini/gemini-3.7-flash`
- Dynamic routes for discovered account/model pairs
- Virtual models such as `llmgateway-auto`, `llmgateway-coding`, and `llmgateway-best`
- Ordered failover and route cooldown for 401/403/429/5xx responses
- OpenAI SSE passthrough and Anthropic SSE translation
- `x-llmgateway-route` response header for route diagnostics
- Embedded local chat/admin UI with no frontend build step
- Local UI backed by server-side SQLite threads
- Automatic migration of v0.3 browser-local threads when the server has no threads yet
- Model picker driven by the live model catalog
- Account/model management and model refresh from the UI
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
              Conversation                  Model catalog
                 store                         store
                   │                             │
            messages / route            account availability
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

## Local UI

Open:

```text
http://127.0.0.1:7331/
```

The UI now uses the persistent thread API. A brand-new chat stays as an in-browser draft until the first message is sent. At that point the gateway creates a server thread and SQLite becomes the source of truth for its history.

The UI supports:

- multiple persistent chat threads
- streaming responses
- `Auto`, virtual policies, and physical model selection
- sticky account/model routing behind each thread
- automatic failover when the sticky route becomes unavailable
- actual route display after each response
- account cards showing which models each account exposes
- refresh model discovery per account
- enable/disable individual account-model bindings
- full model catalog search

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

The request contains only the new message. llmgateway loads prior messages from SQLite, prefers the thread's sticky route, and stores the completed assistant message back into the thread.

Delete a thread:

```bash
curl -X DELETE http://127.0.0.1:7331/v1/threads/<thread_id> \
  -H "Authorization: Bearer $LLMGATEWAY_API_KEY"
```

`POST /v1/threads` also accepts an optional `messages` array, used by the UI to import v0.3 local history.

## Responses API state

`previous_response_id` is supported in v0.4. llmgateway stores the full normalized OpenAI message context plus the selected route for each completed response.

First response:

```bash
curl http://127.0.0.1:7331/v1/responses \
  -H "Authorization: Bearer $LLMGATEWAY_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "model":"llmgateway-coding",
    "input":"Inspect this design"
  }'
```

Then continue with the returned response ID:

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

## Sticky routing

A persistent conversation stores the route that successfully served its last turn:

```text
thread_123
  model: llmgateway-auto
  sticky_route: discovered:qwen-primary:qwen-coder
```

The next turn tries that route first. If it is unavailable, rate-limited, or cooling down, the normal route planner continues through the fallback candidates. The successful replacement route becomes the new sticky route.

The same affinity behavior is used for `previous_response_id` chains.

## API example

```bash
curl http://127.0.0.1:7331/v1/chat/completions \
  -H "Authorization: Bearer $LLMGATEWAY_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "model":"llmgateway-auto",
    "messages":[{"role":"user","content":"Say hello in one sentence"}]
  }'
```

`/v1/chat/completions` remains stateless by design because many external clients already own their full message history. Use `/v1/threads` when llmgateway should own the conversation state.

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

The `gemini/` prefix locks the provider/model choice, but llmgateway still chooses the best enabled Gemini account that exposes that model.

## Claude Code

```bash
export LLMGATEWAY_API_KEY=tmx_change_me
export ANTHROPIC_BASE_URL=http://127.0.0.1:7331
export ANTHROPIC_AUTH_TOKEN="$LLMGATEWAY_API_KEY"
claude
```

## Codex

Use `examples/codex-config.toml`. The provider uses the Responses wire protocol. v0.4 persists Responses chains so clients can continue via `previous_response_id`.

## OpenCode

Use `examples/opencode-config.json`. OpenCode can enumerate `/v1/models` and select virtual or discovered physical models.

## Authentication and security

The compatibility/admin APIs accept either:

```text
Authorization: Bearer <LLMGATEWAY_API_KEY>
```

or:

```text
x-api-key: <LLMGATEWAY_API_KEY>
```

Provider credentials are never returned to clients.

Security defaults:

- bind address is `127.0.0.1`
- compatibility/admin APIs require a gateway API key
- provider keys are loaded from environment variables
- `.env`, active config, and SQLite database files are gitignored
- the embedded UI never receives upstream provider credentials
- do not expose the service publicly without TLS, network controls, and rate limiting

## Failover behavior

Retryable statuses currently include 401, 403, 408, 409, 429, and common 5xx failures. Failover happens before a successful upstream stream is handed to the client. Once output is already streaming, llmgateway does not splice a second model into the same answer.

## Roadmap

1. Conversation checkpoints, rolling summaries, and context compaction.
2. Persistent quota-domain and usage tracking.
3. Browser-session accounts using isolated persistent profiles.
4. Cost, latency, and capability-aware route scoring.
5. Per-client API keys, budgets, and routing policies.
6. Route explanations and usage dashboards.

## License

MIT
