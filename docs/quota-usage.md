# Quota & Usage Engine (v0.10)

`llmgateway` treats an account as a quota domain. A provider can expose many models and an
account can have many routes, but a rate-limit response commonly means that the credential,
project, subscription, or browser identity behind that account needs a cooldown.

## Goals

- persist request/token usage by account and route
- persist account-level 429 cooldown across process restarts
- honor `Retry-After` when an upstream sends it
- observe common OpenAI-compatible `x-ratelimit-remaining-*` headers
- apply optional daily/monthly local budgets
- prefer accounts with more remaining budget when routes otherwise compete
- fail over without requiring the calling client to understand account state

## Configuration

The `[usage]` section is read from the same TOML file as the rest of llmgateway. It is kept as
a sidecar config internally so older `AppConfig` consumers remain compatible.

```toml
[usage]
enabled = true
hard_limits = true
default_rate_limit_cooldown_seconds = 60
balance_weight = 1

[usage.accounts.gemini-primary]
daily_request_limit = 1000
monthly_request_limit = 20000
daily_token_limit = 2000000
monthly_token_limit = 40000000
rate_limit_cooldown_seconds = 120
```

All per-account limits are optional. When a limit is absent it does not contribute to pressure.
`hard_limits = true` removes an account from routing after a configured daily/monthly limit is
reached. When `hard_limits = false`, usage only changes route preference.

`balance_weight` adds a small penalty for each request already sent through an account today.
Set it to `0` when explicit route priorities must remain absolute.

## Routing semantics

The route planner combines three independent layers:

1. configured route priority
2. short-lived route health/circuit-breaker state
3. persistent account quota/usage pressure

For an account with a future `cooldown_until`, `route_penalty()` returns no candidate and every
route using that account is excluded. This is intentionally account-level instead of route-level.

For available accounts the additive pressure includes:

- highest configured daily/monthly usage ratio
- optional daily request balancing weight
- recent 429 penalty
- a soft penalty when provider headers report zero remaining requests/tokens

Lower scores are preferred.

## 429 failover

When an upstream returns HTTP 429:

1. the attempt is recorded in `usage_events`
2. `Retry-After` is read when present
3. the account's `cooldown_until`, `last_429_at`, `consecutive_429`, and error are persisted
4. the individual route is also placed in its normal short-lived router cooldown
5. the gateway tries the next eligible route
6. later requests skip every route belonging to the cooling-down account

The state survives process restarts because it lives in SQLite.

## Stored data

### `usage_events`

One row is written per upstream attempt with:

- account ID
- route ID
- upstream model
- timestamp/status/outcome
- estimated input tokens
- output tokens when available to the recorder
- usage source
- upstream/transport error

v0.10 always records a conservative request-input estimate so streaming and compatibility
protocols have a common baseline. The store also supports replacing estimates with provider
reported `usage` values; full streaming-usage normalization can be added per adapter without
changing the database schema.

### `account_quota_state`

Stores:

- cooldown deadline
- last 429 timestamp
- consecutive 429 count
- common remaining-request/token hints
- last error

A successful request clears the transient account cooldown/429 streak but leaves usage history
intact.

## Admin APIs

### All accounts

```text
GET /_llmgateway/usage
```

Returns daily/monthly counters, limits, pressure, cooldown and remaining hints for enabled
accounts.

### One account

```text
GET /_llmgateway/accounts/{account_id}/usage
```

### Clear transient quota state

```text
POST /_llmgateway/accounts/{account_id}/quota/reset
```

Reset removes cooldown, 429 streak, remaining hints and the last error. It deliberately does not
delete usage events or reset configured daily/monthly budgets.

## CI invariant

`smoke-quota.sh` runs two fake accounts against the same provider. The primary credential always
returns 429 with `Retry-After`; the secondary succeeds. The test verifies:

- the first request falls through to the secondary route
- the primary account becomes blocked with a persisted 429 state
- the second request skips the primary entirely
- per-account request/token counters are visible through the admin API
- provider remaining hints are persisted
- manual transient-state reset works without erasing usage history
