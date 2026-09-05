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

`POST /_llmgateway/routes/explain` includes `group_tier_priority` for every candidate in a tiered group. This makes fallback order visible without exposing provider-private model metadata.

## Future extensions

The tier model is intentionally small. Follow-up work can add per-tier strategies such as weighted, round-robin, least-loaded or lowest-latency selection, capability requirements, group-to-group fallback and a drag/drop admin editor without changing the hard-tier contract.


## Admin UI and CRUD API

The built-in UI includes a **Groups** view. Choose **Create group** to define a stable group ID, add ordered fallback tiers, and assign configured routes without editing TOML by hand.

Saving a group:

- validates all tier priorities and route references;
- persists the active `virtual_models` / `model_groups` namespace with a backup;
- hot-swaps the live gateway configuration;
- exposes the new logical model through `/v1/models` immediately, without a restart.

Admin endpoints:

- `GET /_llmgateway/model-groups` — list groups plus configured route inventory;
- `POST /_llmgateway/model-groups` — create a tiered group;
- `PUT /_llmgateway/model-groups/{group_id}` — replace the group's tiers;
- `DELETE /_llmgateway/model-groups/{group_id}` — delete a non-default group.

The default group cannot be deleted. A group targeted by an alias must be detached from that alias before deletion.

The browser-account wizard is tier-aware: when it auto-adds a new route to an existing tiered built-in group, it appends that route to the lowest-priority fallback tier instead of creating an invalid mixed `routes` + `tiers` configuration.
