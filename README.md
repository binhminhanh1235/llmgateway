# llmgateway

A local-first universal LLM gateway written in Rust. Point coding agents and OpenAI/Anthropic-compatible clients at one endpoint, or use the built-in local chat UI, while llmgateway selects an account/model route and fails over when an upstream becomes unavailable or rate-limited.

## What exists in v0.3

- OpenAI-compatible `POST /v1/chat/completions`
- OpenAI-compatible `POST /v1/responses` for clients such as Codex
- Anthropic-compatible `POST /v1/messages` for clients such as Claude Code
- OpenAI-compatible `GET /v1/models`
- Multi-provider and multi-account configuration
- SQLite-backed model catalog
- Per-account model discovery through provider `/models` endpoints
- Per-account availability: `available`, `unknown`, `unavailable`
- Enable/disable a model for an individual account
- Canonical physical model IDs such as `gemini/gemini-3.7-flash`
- Dynamic routes for discovered account/model pairs
- Virtual models such as `llmgateway-auto`, `llmgateway-coding`, and `llmgateway-best`
- Ordered failover and lightweight route cooldown for 401/403/429/5xx responses
- OpenAI SSE passthrough and Anthropic SSE translation
- `x-llmgateway-route` response header for route diagnostics
- Embedded local chat/admin UI with no frontend build step
- Local chat threads with independent context in browser storage
- Model picker driven by the live model catalog
- Account/model management and model refresh from the UI
- Example configurations for Claude Code, Codex, and OpenCode

The gateway currently uses OpenAI Chat Completions as the common upstream protocol. OpenAI Responses and Anthropic Messages are translated into that shape before routing.

## Architecture

```text
Claude Code ─ Anthropic Messages ─┐
Codex ───── OpenAI Responses ─────┤
OpenCode ───── OpenAI Chat ───────┤
Local UI ───── OpenAI Chat ───────┤
                                  ▼
                           ┌──────────────┐
                           │  llmgateway  │
                           └──────┬───────┘
                                  │
                         virtual / physical model
                                  │
                           SQLite model catalog
                                  │
                     account-model availability
                                  │
                           route planning
                                  │
             ┌────────────────────┼────────────────────┐
             ▼                    ▼                    ▼
        Gemini acct A        Qwen acct A        OpenRouter
        Gemini acct B        Qwen acct B        other APIs
```

## Quick start

### 1. Configure

```bash
cp config/llmgateway.example.toml config/llmgateway.toml
cp .env.example .env
```

Add at least one upstream credential and set `LLMGATEWAY_API_KEY` in `.env`.

The default catalog database is:

```text
data/llmgateway.db
```

You can change it with:

```toml
[storage]
database_url = "sqlite://data/llmgateway.db"
```

### 2. Run

```bash
cargo run --release
```

Default endpoint:

```text
http://127.0.0.1:7331
```

### 3. Open the local UI

Open:

```text
http://127.0.0.1:7331/
```

Enter `LLMGATEWAY_API_KEY` when prompted. The UI can keep the key for the browser session or, only when explicitly selected, remember it in local browser storage.

The UI supports:

- multiple chat threads
- separate message context for each thread
- streaming responses
- `Auto`, virtual policies, and physical model selection
- automatic account selection after choosing a physical model
- actual route display after each response
- account cards showing which models each account exposes
- refresh model discovery per account
- enable/disable individual account-model bindings
- full model catalog search

v0.3 chat threads are stored client-side in browser `localStorage`. Each request sends that thread's message history, so context remains isolated per thread. Server-side persistent threads and checkpoints are planned for a later release.

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

## Model catalog and discovery

Configured route models are seeded into SQLite at startup. They begin with `unknown` availability until provider discovery verifies them.

List accounts:

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

Show that account's models:

```bash
curl http://127.0.0.1:7331/_llmgateway/accounts/gemini-primary/models \
  -H "Authorization: Bearer $LLMGATEWAY_API_KEY"
```

Enable or disable a model for one account:

```bash
curl -X PATCH \
  http://127.0.0.1:7331/_llmgateway/accounts/gemini-primary/models \
  -H "Authorization: Bearer $LLMGATEWAY_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "model_id":"gemini/gemini-3.7-flash",
    "enabled":false
  }'
```

Show the full catalog:

```bash
curl http://127.0.0.1:7331/_llmgateway/models \
  -H "Authorization: Bearer $LLMGATEWAY_API_KEY"
```

`GET /v1/models` exposes:

- virtual policies, for example `llmgateway-auto`
- canonical physical models, for example `gemini/gemini-3.7-flash`
- older explicit route IDs retained for client compatibility

### Choose a model, not an account

```bash
curl http://127.0.0.1:7331/v1/chat/completions \
  -H "Authorization: Bearer $LLMGATEWAY_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "model":"gemini/gemini-3.7-flash",
    "messages":[{"role":"user","content":"Explain Kafka zero copy"}]
  }'
```

The `gemini/` prefix locks the provider/model choice, but llmgateway still chooses the best enabled Gemini account that exposes the model. If that account fails with a retryable error, another enabled account for the same physical model may take over.

## Provider discovery configuration

```toml
[[providers]]
id = "openrouter"
kind = "openai-compatible"
base_url = "https://openrouter.ai/api/v1"
models_path = "models"

[[accounts]]
id = "openrouter-main"
provider = "openrouter"
api_key_env = "OPENROUTER_API_KEY"
enabled = true
discover_models = true
```

Discovery supports common OpenAI model-list responses (`data: [...]`) and Gemini-style native responses (`models: [...]`). Raw model metadata is retained in SQLite for richer capability/routing logic later.

## Virtual models

A virtual model is a routing policy rather than a concrete upstream model.

```toml
[virtual_models.llmgateway-coding]
routes = ["qwen-coder", "gemini-primary", "openrouter-fallback"]
```

If a route receives a retryable failure such as `429`, llmgateway cools that route down and tries the next candidate.

## Claude Code

```bash
export LLMGATEWAY_API_KEY=tmx_change_me
export ANTHROPIC_BASE_URL=http://127.0.0.1:7331
export ANTHROPIC_AUTH_TOKEN="$LLMGATEWAY_API_KEY"
claude
```

Model aliases let Claude Code continue requesting familiar Claude model names while llmgateway maps them to routing policies.

## Codex

Use `examples/codex-config.toml`. The provider uses the Responses wire protocol. `previous_response_id` is still intentionally unsupported until server-side response/thread state is implemented.

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

Retryable statuses currently include 401, 403, 408, 409, 429, and common 5xx failures. Failover happens before a successful upstream stream is handed to the client. Once output is already streaming, v0.3 does not splice a second model into the same answer.

## Roadmap

1. Persistent server-side threads, checkpoints, and `previous_response_id`.
2. Sticky account routing and quota-domain/usage persistence.
3. Browser-session accounts using isolated persistent profiles.
4. Cost, latency, and capability-aware route scoring.
5. Per-client API keys, budgets, and routing policies.
6. Richer UI controls for priorities, usage, and route explanations.

## License

MIT
