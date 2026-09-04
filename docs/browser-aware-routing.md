# Browser-aware routing intelligence

v0.31 turns multiple browser accounts into one predictable local execution pool without weakening the existing readiness, quota, task-fit, or API-fallback rules.

## Decision order

For a virtual model, Router evaluates each candidate in this order:

```text
hard eligibility
  account/session/adapter/model ready
  route enabled
  cooldown/quota hard limits
  known context window can fit request
  execution policy hard boundary
        ↓
transport preference
        ↓
base priority
+ quota pressure
+ adaptive latency/reliability penalty
+ browser recovery penalty
+ task-fit adjustment
        ↓
browser-only fairness tie-break
        ↓
stable config order
```

Lower scores win. Browser fairness is deliberately only a tie-break between browser routes after transport policy and score are equal. It never makes a browser route beat an API route in `balanced` mode merely because the browser was used less recently.

## Execution policies

`routing.execution_preference` accepts:

| Policy | Behavior |
| --- | --- |
| `prefer-browser` | Rank eligible browser routes before API routes. API remains fallback when `api_fallback = true`. |
| `browser-only` | Exclude API routes for virtual-model execution. |
| `balanced` | No browser/API transport preference. Normal score decides. |
| `prefer-api` | Rank eligible API routes before browser routes. |
| `api-only` | Exclude browser routes for virtual-model execution. |

For compatibility, `browser-first` is an alias of `prefer-browser`, and `api-first` is an alias of `prefer-api`.

The default remains browser-first behavior.

## Browser account fairness

When `browser_fairness_enabled = true`, Router records the sequence of successful browser-account use in memory. Among equal-quality browser routes, the least recently successful account wins.

Example with two equally healthy Gemini accounts:

```text
request 1 -> Gemini A
request 2 -> Gemini B
request 3 -> Gemini A
```

Fairness does not override configured priority, quota pressure, task fit, adaptive reliability, recovery penalty, transport policy, or hard readiness.

The persistent quota store still applies its existing account-level usage/quota penalty. This means provider quota hints or configured local limits remain stronger routing signals than LRU rotation.

## Session-aware sticky affinity

Persistent threads already remember the last successful route. v0.31 makes browser affinity explicit with:

```toml
[routing]
browser_sticky_affinity = true
```

A healthy eligible browser route may stay attached to the thread even when LRU fairness would otherwise rotate to an equal peer. Sticky affinity cannot resurrect an ineligible route, cross a hard execution-policy boundary, or override a materially better task fit.

Set `browser_sticky_affinity = false` to let each turn participate in normal browser rotation.

## Recovery scoring

A route inside cooldown is excluded as before. After cooldown expires, a browser route with recent consecutive failures gets a bounded additive penalty:

```text
browser_recovery_penalty =
    min(consecutive_failures * configured_penalty, configured_max)
```

Defaults:

```toml
browser_recovery_penalty = 8
browser_recovery_max_penalty = 40
```

A successful request clears route health and therefore clears the recovery penalty. This prevents a recently unstable browser from immediately jumping ahead of a healthy peer while still allowing deterministic recovery probes.

## Browser model capability and context metadata

Configured virtual-model routes are enriched from Model Catalog when matching provider/account/model metadata is available. Missing route capabilities are merged and a missing `context_window` can be filled from the catalog.

The existing task-aware router then applies those facts normally:

- coding/reasoning/long-context/fast capability fit;
- known context-window enforcement;
- hard exclusion when the known context window cannot fit the request.

Unknown metadata remains neutral for backward compatibility.

## Route explain

`POST /_llmgateway/routes/explain` now includes browser-aware fields on each candidate:

- `policy_reason`
- `browser_fairness_rank`
- `browser_recovery_penalty`
- normalized top-level `execution_policy`

Typical reasons include `browser_preferred`, `api_fallback`, `browser_only`, `api_preferred`, `browser_fallback`, and `api_only`.

Hard-policy exclusions use `policy_browser_only` or `policy_api_only`. When `prefer-browser` is configured with `api_fallback = false`, API candidates show `api_fallback_disabled`.

## Deterministic fallback

Under `prefer-browser`, all eligible browser routes are attempted before API routes. A retryable browser failure continues through the remaining browser pool first; paid/credentialed API execution is reached only after browser candidates are exhausted or unavailable.

Use `browser-only` when API spend must never be used for that virtual-model policy.

## CI contract

The v0.31 smoke gate uses two isolated fake Chromium accounts plus a fake API provider. It proves:

- equal-quality browser-account rotation;
- persistent-thread browser sticky affinity;
- retryable browser-to-browser failover;
- cooldown exclusion and cautious post-cooldown recovery;
- deterministic browser-to-API fallback;
- explain metadata for policy, fairness, and recovery.

No live Gemini/Qwen website is required for CI.
