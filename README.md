# llmgateway

A local-first universal LLM gateway written in Rust. Point coding agents and OpenAI/Anthropic-compatible clients at one endpoint, then let the gateway select an account/model route and fail over when an upstream becomes unavailable or rate-limited.

## What exists in v0.1

- OpenAI-compatible `POST /v1/chat/completions`
- OpenAI-compatible `POST /v1/responses` (stateless subset for coding agents such as Codex)
- Anthropic-compatible `POST /v1/messages`
- `GET /v1/models`
- Multi-provider and multi-account configuration
- Virtual models such as `llmgateway-auto` and `llmgateway-coding`
- Model aliases for clients such as Claude Code
- Ordered failover across routes
- Lightweight circuit-breaker cooldown for 401/403/429/5xx responses
- OpenAI SSE passthrough
- Anthropic SSE translation for text and tool calls
- Per-response `x-llmgateway-route` debug header
- Health endpoint at `/_llmgateway/health`
- Example configs for Claude Code, Codex and OpenCode

The first release uses OpenAI Chat Completions as the upstream common denominator. The gateway translates OpenAI Responses and Anthropic Messages requests into that canonical upstream shape. Browser-session accounts, persistent thread context, cost-aware routing and a UI are planned next.

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
                            model aliases
                                  │
                           virtual models
                                  │
                           ordered routes
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

## Virtual models

A virtual model is a routing policy, not a concrete upstream model.

```toml
[virtual_models.llmgateway-coding]
routes = ["qwen-coder", "gemini-primary", "openrouter-fallback"]
```

If the first route receives a retryable error such as `429`, the gateway cools that route down and tries the next route.

## Multi-account routing

Providers define endpoints; accounts define credentials; routes bind an account to a concrete model.

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

This separation is intentional: adding a second credential does not require duplicating provider configuration.

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

Copy `examples/codex-config.toml` into your Codex configuration, or merge the provider section into your existing config. The example uses `wire_api = "responses"`, which current Codex custom providers expect. v0.1 supports the stateless Responses subset used for messages, function tools, tool outputs and SSE streaming; `previous_response_id` is intentionally rejected until server-side response state is implemented.

## OpenCode

Use `examples/opencode-config.json` as a starting point. It registers llmgateway as a custom OpenAI-compatible provider.

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

Once an SSE response has started flowing to the client, v0.1 does not attempt cross-model continuation. This avoids stitching two different model outputs into one corrupted agent turn.

## Security

- Default bind address is `127.0.0.1`.
- The public compatibility APIs require a gateway API key.
- Provider API keys are read from environment variables.
- `.env` and the active config file are gitignored.
- Do not expose the service publicly without TLS, network controls and rate limiting.

## Roadmap

1. Stateful Responses API (`previous_response_id`) and broader built-in tool coverage.
2. Native provider adapters and capability negotiation.
3. Browser-session accounts for supported services, using isolated persistent profiles rather than copying raw cookies.
4. Account pools with sticky routing, quota domains and weighted/least-used strategies.
5. Cost, latency and capability-aware route scoring.
6. Persistent threads, context checkpoints and model-independent memory.
7. Local admin UI for accounts, routing, usage and debugging.
8. Per-client API keys, budgets and policies.

## License

MIT
