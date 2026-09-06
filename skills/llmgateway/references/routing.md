# Routing and model selection

## Agent responsibility vs gateway responsibility

The agent chooses intent. llmgateway chooses an eligible route.

The agent should usually decide:

- task/protocol;
- logical model/group;
- optional hard user constraint.

llmgateway already decides:

- route/account readiness;
- enabled state;
- client policy;
- model availability;
- browser/API policy;
- group tier priority;
- quota/cooldown;
- adaptive reliability/latency;
- task fit;
- browser recovery;
- fairness;
- retry/fallback eligibility.

Do not reimplement these rules in prompt logic.

## Selection algorithm

1. Call `GET /_llmgateway/agent/capabilities` with the execution credential.
2. If the user requested an exact visible model, keep that model constraint.
3. Otherwise prefer a logical model/group such as a visible `llmgateway-*` model.
4. Express hard semantic requirements through `POST /_llmgateway/agent/resolve`.
5. Execute with the same `llmgateway_requirements` so execution cannot drift from resolution.
6. Use `/_llmgateway/routes/explain` only for deeper admin diagnostics.

## Ordered fallback groups

Tiered model groups are hard-priority fallbacks.

Lower numeric tier priority wins. llmgateway exhausts eligible/retryable work in the higher tier before falling to the next tier.

Disabled accounts or models stay configured but are ignored for fallback eligibility. Re-enabling them can restore eligibility without reconstructing the group.

Therefore an agent must not "repair" a group by deleting an ignored member just because it is currently unavailable.

## Client policies

A client credential can restrict:

- model IDs/patterns;
- route IDs;
- browser/API execution policy;
- API fallback;
- daily/monthly request budgets;
- daily/monthly token budgets.

A request-level routing override can narrow permissions but cannot broaden the client's configured boundary.

If `/v1/models` does not show a model, do not try to bypass policy through a physical route ID.

## Capability handling

Treat current advertised metadata as authoritative when present.

`llmgateway_requirements.capabilities` are **hard eligibility constraints**, not scoring hints. Capability names are normalized to lowercase and underscores become hyphens.

`min_context_window` is also a hard requirement. A known smaller context window is excluded with `minimum_context_window_not_met`; unknown context metadata is conservatively excluded with `context_window_unknown`.

These constraints run inside the existing Router before ranking. They do not bypass model groups, client policies, readiness, quota, transport policy, health, task fit, fairness, or fallback tiers.

Never infer vision/file/audio/image-generation support solely from a provider or model family name. When a required capability is not advertised by any eligible route, report the capability gap rather than guessing.
