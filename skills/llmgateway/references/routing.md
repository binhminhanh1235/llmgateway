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

1. Call `GET /v1/models` with the execution credential.
2. If the user requested an exact visible model, use it.
3. Otherwise prefer a suitable logical model/group such as a visible `llmgateway-*` model.
4. If several logical models are available, choose by semantic purpose, not provider brand.
5. Use `/_llmgateway/routes/explain` only for diagnostics or when the decision materially benefits from explaining route candidates.
6. If no suitable logical model exists, choose a visible physical model that satisfies the explicit requirement.

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

Never infer vision/file/audio/image-generation support solely from a provider or model family name.

When a modality is required and no visible eligible model advertises it, stop and report the capability gap.
