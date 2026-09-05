# Model Groups and Ordered Fallback

llmgateway virtual models are logical model groups. Existing flat groups remain supported:

```toml
[virtual_models.llmgateway-auto]
routes = ["gemini-primary", "qwen-coder", "openrouter-fallback"]
```

For strict fallback ordering, define priority tiers:

```toml
[[virtual_models.llmgateway-coding.tiers]]
priority = 10
routes = ["chatgpt-primary", "gemini-pro"]

[[virtual_models.llmgateway-coding.tiers]]
priority = 20
routes = ["qwen-coder", "deepseek"]

[[virtual_models.llmgateway-coding.tiers]]
priority = 30
routes = ["gemini-flash"]
```

The top-level `model_groups` name is also accepted as an alias for `virtual_models`. Do not define both top-level names in the same TOML document.

## Selection semantics

Tier priority is a hard boundary. Lower numeric values are preferred.

1. Resolve aliases and the requested logical model.
2. Expand the model group into configured routes.
3. Apply route/account readiness, model availability, transport policy, task fit, cooldown and quota eligibility.
4. Rank eligible routes by group tier first.
5. Inside the same tier, keep the existing transport preference, configured route priority, quota, adaptive, browser recovery, task-aware and fairness behavior.
6. On a retryable execution failure, continue through the remaining routes in the same tier before falling through to the next tier.

A lower tier cannot jump ahead merely because it currently has a lower adaptive score or faster latency.

## Backward compatibility

Flat `routes = [...]` groups preserve the pre-tier routing behavior. They are treated as un-tiered and continue to use the existing scoring system globally.

A group must use either flat `routes` or `tiers`, never both.

## Validation

llmgateway rejects:

- empty model groups;
- negative tier priorities;
- empty tiers;
- duplicate tier priority values;
- unknown route IDs;
- a route listed more than once or in more than one tier;
- groups that mix legacy `routes` with `tiers`.

## Observability

`POST /_llmgateway/routing/explain` includes `group_tier_priority` for every candidate in a tiered group. This makes fallback order visible without exposing provider-private model metadata.

## Future extensions

The tier model is intentionally small. Follow-up work can add per-tier strategies such as weighted, round-robin, least-loaded or lowest-latency selection, capability requirements, group-to-group fallback and a drag/drop admin editor without changing the hard-tier contract.
