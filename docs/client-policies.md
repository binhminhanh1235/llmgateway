# Client policies and budgets

v0.32 adds a client-policy layer in front of the existing Router. It does not create a second routing engine. After a client is authenticated, llmgateway derives an immutable request-level config snapshot, applies the client's boundaries, and then sends the request through the normal readiness, quota, task-fit, adaptive, browser-fairness, recovery, and failover pipeline.

## Credential model

The global `LLMGATEWAY_API_KEY` remains the unrestricted admin and legacy credential. Optional client credentials are configured by environment-variable name:

```toml
[clients.claude-code]
key_env = "LLMGATEWAY_CLAUDE_CODE_KEY"
enabled = true
```

Then set the value outside the config file:

```bash
export LLMGATEWAY_CLAUDE_CODE_KEY="replace-with-a-random-local-secret"
```

Enabled clients must have their configured key environment variable available when llmgateway starts. Admin/client credentials must be non-empty and unique; a client key cannot equal the global admin key or another enabled client key. Raw client key values are never returned by the diagnostics API.

Client keys are accepted by:

```text
POST /v1/chat/completions
POST /v1/responses
POST /v1/messages
GET  /v1/models
```

The global key continues to work on those endpoints and remains required for admin and persistent-thread management APIs.

## Model and route boundaries

`allowed_models` matches either the requested model or the alias-resolved model. An empty list preserves the backward-compatible meaning of "no model restriction."

```toml
[clients.codex]
key_env = "LLMGATEWAY_CODEX_KEY"
allowed_models = ["llmgateway-coding", "llmgateway-auto"]
```

Patterns use the same simple wildcard matching as model aliases. A denied model fails with HTTP 403 before provider execution. `GET /v1/models` is filtered through the same model boundary.

`allowed_routes` is optional and acts as a second boundary after model resolution:

```toml
allowed_routes = ["gemini-web-*", "qwen-web-*"]
```

This is useful when two clients may request the same virtual model but one of them must never execute on a paid or sensitive route.

## Browser/API transport policy

A client can override the global routing defaults:

```toml
execution_preference = "prefer-browser"
api_fallback = false
```

Supported values are `prefer-browser`, `browser-only`, `balanced`, `prefer-api`, and `api-only`. The older `browser-first` and `api-first` names remain accepted aliases.

For a browser-first local workflow:

- `prefer-browser` + `api_fallback = true` tries eligible browser routes first and permits deterministic API fallback.
- `prefer-browser` + `api_fallback = false` keeps API routes out of fallback.
- `browser-only` is the hard boundary even if the global gateway permits APIs.
- `api-only` is the corresponding hard API boundary.

Requests may include the llmgateway-only controls `llmgateway_execution_preference` and `llmgateway_api_fallback`. They are stripped before the request reaches the provider. A client may use them to become more restrictive for one request, but never to widen its configured transport permissions. An attempted privilege expansion fails with HTTP 403.

## Request and token budgets

Budgets are optional and are tracked per client in the same SQLite database as the rest of llmgateway state:

```toml
daily_request_limit = 2000
monthly_request_limit = 40000
daily_token_limit = 10000000
monthly_token_limit = 200000000
```

Daily and monthly windows use UTC calendar boundaries.

Request budget accounting is reservation-based. Before provider execution, llmgateway reserves one request plus normalized request token usage:

```text
estimated input tokens
+ caller-declared max_output_tokens / max_completion_tokens / max_tokens
```

For a client with a token budget, an omitted output cap is not treated as zero. llmgateway injects a bounded `max_tokens` value before provider execution. The bound is the smaller of the remaining daily/monthly token capacity and a conservative 4096-token default. If there is no safe output capacity left, admission fails before provider execution.

When a completed provider response reports usage, llmgateway reconciles the reservation to observed input/output token counts. OpenAI-style `prompt_tokens` / `completion_tokens` and Responses-style `input_tokens` / `output_tokens` are normalized into the same client budget ledger. If observed usage is unavailable, such as an interrupted stream, the conservative request reservation remains. Both reservations and reconciled usage persist across gateway restarts.

This keeps hard budgets deterministic at admission time while avoiding permanent over-accounting after observed usage becomes available. A budget rejection returns HTTP 429 with error type `client_budget_exceeded`.

## Diagnostics

Use the global admin key:

```bash
curl http://127.0.0.1:7331/_llmgateway/clients \
  -H "Authorization: Bearer $LLMGATEWAY_API_KEY"
```

The response exposes client IDs, enabled state, whether the configured key environment variable exists, model/route policy, routing settings, and daily/monthly budget usage, limits, and explicit remaining request/token amounts. It does not return the client secret value.

Route explain can evaluate a specific client policy:

```bash
curl -X POST http://127.0.0.1:7331/_llmgateway/routes/explain \
  -H "Authorization: Bearer $LLMGATEWAY_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{"model":"llmgateway-auto","client_id":"claude-code"}'
```

Policy-denied candidates include explicit reasons such as `client_policy_model_forbidden`, `client_policy_route_forbidden`, `policy_browser_only`, or `api_fallback_disabled`.

## Suggested local-tool presets

### Claude Code

```toml
[clients.claude-code]
key_env = "LLMGATEWAY_CLAUDE_CODE_KEY"
allowed_models = ["claude-*", "llmgateway-coding", "llmgateway-best"]
execution_preference = "prefer-browser"
api_fallback = true
```

Set `ANTHROPIC_BASE_URL=http://127.0.0.1:7331` and use the client key as the Anthropic auth token.

### Codex

```toml
[clients.codex]
key_env = "LLMGATEWAY_CODEX_KEY"
allowed_models = ["llmgateway-coding", "llmgateway-auto"]
execution_preference = "prefer-browser"
api_fallback = true
```

Codex continues to use the OpenAI Responses compatibility endpoint.

### OpenCode

```toml
[clients.opencode]
key_env = "LLMGATEWAY_OPENCODE_KEY"
allowed_models = ["llmgateway-*"]
execution_preference = "prefer-browser"
api_fallback = true
```

OpenCode sees a policy-filtered `/v1/models` list.

### OmniVoiceStudio

```toml
[clients.omnivoicestudio]
key_env = "LLMGATEWAY_OMNIVOICE_KEY"
allowed_models = ["llmgateway-auto"]
execution_preference = "prefer-browser"
api_fallback = false
daily_request_limit = 5000
```

This preset deliberately avoids silent paid API fallback. Enable fallback explicitly only when that behavior is desired.

## Responses state isolation

`previous_response_id` state is owned by the authenticated client that created it. A client credential can continue only its own Responses chain. Cross-client and legacy/unowned context lookups return the same not-found response so they do not reveal whether another client's response ID exists. The unrestricted admin/legacy credential remains able to continue existing response contexts for backward compatibility.

## Security notes

- Keep client keys in environment variables or a local secret manager, not committed config.
- Use the global key only for administration or trusted legacy clients.
- Restrict model and route lists when a client should have a narrow execution surface.
- Client policy is an authorization boundary inside llmgateway; it is not a substitute for TLS, host firewalling, or network access controls when exposing the service beyond localhost.
