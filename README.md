# llmgateway

A local-first universal LLM gateway written in Rust. Point coding agents and OpenAI/Anthropic-compatible clients at one endpoint, then let the gateway select an account/model route and fail over when an upstream becomes unavailable or rate-limited.

## What exists in v0.2

- OpenAI-compatible `POST /v1/chat/completions`
- OpenAI-compatible `POST /v1/responses` (stateless subset for coding agents such as Codex)
- Anthropic-compatible `POST /v1/messages`
- OpenAI-compatible `GET /v1/models`
- Multi-provider and multi-account configuration
- SQLite-backed model catalog
- Per-account model discovery through provider `/models` endpoints
- Per-account model availability: `available`, `unknown`, `unavailable`
- Canonical physical model IDs such as `gemini/gemini-3.7-flash` and `openrouter/anthropic/claude-sonnet-4.6`
- Dynamic routes for discovered account/model pairs, even when no explicit route was declared
- Virtual models such as `llmgateway-auto` and `llmgateway-coding`
- Model aliases for clients such as Claude Code
- Ordered failover across routes
- Lightweight circuit-breaker cooldown for 401/403/429/5xx responses
- OpenAI SSE passthrough
- Anthropic SSE translation for text and tool calls
- Per-response `x-llmgateway-route` debug header
- Model/account admin APIs
- Health endpoint at `/_llmgateway/health`
- Example configs for Claude Code, Codex and OpenCode

The gateway currently uses OpenAI Chat Completions as the upstream common denominator. The gateway translates OpenAI Responses and Anthropic Messages requests into that canonical upstream shape. Browser-session accounts, persistent thread context, cost-aware routing and a local UI are planned next.

## Architecture

```text
Claude Code ─ Anthropic Messages ─┐
Codex ───── OpenAI Responses ─────┤
OpenCode ───── OpenAI Chat ───────┤
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

Add at least one upstream credential to `.env`, then remove or disable routes/accounts you do not use.

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

### 3. Test

```bash
curl http://127.0.0.1:7331/v1/chat/completions \
  -H "Authorization: Bearer $LLMGATEWAY_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "model":"llmgateway-auto",
    "messages":[{"role":"user","content":"Say hello in one sentence"}]
  }'
```

Inspect route health:

```bash
curl http://127.0.0.1:7331/_llmgateway/health
```

## Model catalog and discovery

Configured route models are seeded into SQLite at startup. They initially have `unknown` availability until a provider discovery call verifies them.

List accounts:

```bash
curl http://127.0.0.1:7331/_llmgateway/accounts \
  -H "Authorization: Bearer $LLMGATEWAY_API_KEY"
```

Refresh models for one account:

```bash
curl -X POST \
  http://127.0.0.1:7331/_llmgateway/accounts/gemini-primary/models/refresh \
  -H "Authorization: Bearer $LLMGATEWAY_API_KEY"
```

Show the models available to that account:

```bash
curl http://127.0.0.1:7331/_llmgateway/accounts/gemini-primary/models \
  -H "Authorization: Bearer $LLMGATEWAY_API_KEY"
```

Show the full catalog with account bindings and capabilities:

```bash
curl http://127.0.0.1:7331/_llmgateway/models \
  -H "Authorization: Bearer $LLMGATEWAY_API_KEY"
```

`GET /v1/models` exposes three kinds of selectable IDs:

- virtual policies, for example `llmgateway-auto`
- canonical physical models, for example `gemini/gemini-3.7-flash`
- v0.1 route IDs, retained for compatibility

### Select a physical model while keeping automatic account selection

```bash
curl http://127.0.0.1:7331/v1/chat/completions \
  -H "Authorization: Bearer $LLMGATEWAY_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "model":"gemini/gemini-3.7-flash",
    "messages":[{"role":"user","content":"Explain zero copy in Kafka"}]
  }'
```

The `gemini/` prefix locks the provider/model choice, but llmgateway still chooses the best enabled Gemini account that exposes that model. If the selected account fails with a retryable error, another account for the same model can take over.

If a discovered model has no explicit route in TOML, llmgateway creates an in-memory dynamic route for the account/model pair. This lets newly discovered models become selectable without editing the route list first.

## Provider discovery configuration

Each provider can specify the model-list path:

```toml
[[providers]]
id = "openrouter"
kind = "openai-compatible"
base_url = "https://openrouter.ai/api/v1"
models_path = "models"
```

Each account can opt in or out of discovery:

```toml
[[accounts]]
id = "openrouter-main"
provider = "openrouter"
api_key_env = "OPENROUTER_API_KEY"
enabled = true
discover_models = true
```

Discovery supports the common OpenAI shape (`data: [...]`) and a Gemini-style native shape (`models: [...]`). Raw provider model metadata is retained in SQLite so richer UI/capability logic can be added without changing the schema.

## Virtual models

A virtual model is a routing policy, not a concrete upstream model.

```toml
[virtual_models.llmgateway-coding]
routes = ["qwen-coder", "gemini-primary", "openrouter-fallback"]
```

If the first route receives a retryable error such as `429`, the gateway cools that route down and tries the next route.

## Multi-account routing

Providers define endpoints; accounts define credentials/quota domains; routes bind an account to a concrete preferred model.

```toml
[[accounts]]
id = "gemini-primary"
provider = "gemini"
api_key_env = "GEMINI_API_KEY_PRIMARY"
enabled = true

[[accounts]]
id = "gemini-secondary"
provider = "gemini"
api_key_env = "GEMINI_API_KEY_SECONDARY"
enabled = true
```

This separation is intentional: adding a second credential does not require duplicating provider configuration. v0.2 additionally stores model availability per account, so two accounts under the same provider are allowed to expose different model sets.

## Claude Code

`llmgateway` exposes Anthropic Messages at `/v1/messages`.

```bash
export LLMGATEWAY_API_KEY=tmx_change_me
export ANTHROPIC_BASE_URL=http://127.0.0.1:7331
export ANTHROPIC_AUTH_TOKEN="$LLMGATEWAY_API_KEY"
claude
```

The example config includes aliases such as:

```toml
[[aliases]]
pattern = "claude-sonnet-*"
target = "llmgateway-coding"
```

So Claude Code can keep requesting a Claude-shaped model name while llmgateway routes the request according to your policy.

## Codex

Copy `examples/codex-config.toml` into your Codex configuration, or merge the provider section into your existing config. The example uses `wire_api = "responses"`. v0.2 supports the stateless Responses subset used for messages, function tools, tool outputs and SSE streaming; `previous_response_id` is intentionally rejected until server-side response state is implemented.

## OpenCode

Use `examples/opencode-config.json` as a starting point. It registers llmgateway as a custom OpenAI-compatible provider. Clients that enumerate `/v1/models` can now see virtual policies and discovered physical models.

## API authentication

The gateway accepts either:

```text
Authorization: Bearer <LLMGATEWAY_API_KEY>
```

or:

```text
x-api-key: <LLMGATEWAY_API_KEY>
```

Upstream credentials are never returned to clients.

## Failover behavior

Automatic failover is attempted before a successful upstream response is returned. Retryable HTTP statuses currently include:

- 401 / 403: credential/account temporarily disabled
- 408 / 409: transient request failure
- 429: rate/quota limit
- 5xx: upstream outage

Once an SSE response has started flowing to the client, v0.2 does not attempt cross-model continuation. This avoids stitching two different model outputs into one corrupted agent turn.

## Security

- Default bind address is `127.0.0.1`.
- The public compatibility APIs require a gateway API key.
- Provider API keys are read from environment variables.
- `.env`, the active config file and SQLite database files are gitignored.
- Do not expose the service publicly without TLS, network controls and rate limiting.

## Roadmap

1. Local admin/chat UI with account cards, model picker and refresh controls.
2. Enable/disable and priority controls for account-model bindings.
3. Sticky account routing and quota-domain/usage persistence.
4. Stateful Responses API (`previous_response_id`) and broader built-in tool coverage.
5. Browser-session accounts for supported services, using isolated persistent profiles rather than copying raw cookies.
6. Cost, latency and capability-aware route scoring.
7. Persistent threads, context checkpoints and model-independent memory.
8. Per-client API keys, budgets and policies.

## License

MIT
