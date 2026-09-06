# Diagnostics decision tree

Use evidence from llmgateway before changing state.

## 1. Gateway unavailable

Check `GET /_llmgateway/health`.

If unreachable, report a gateway/process/network problem. Do not blame a provider yet.

## 2. Requested model is missing

Check `GET /v1/models` with the same client key.

Possible classes:

- client policy excludes it;
- model/group is disabled;
- no enabled/available account binding;
- provider discovery has changed;
- logical group is disabled.

With admin access, inspect models, accounts and model groups.

## 3. Model exists but route fails

Run route explain for the same logical model and representative task.

Inspect exclusion/reason fields for:

- client policy;
- browser-only/API-only boundary;
- API fallback disabled;
- readiness;
- quota/cooldown;
- model availability;
- group tier;
- route/account health.

Do not manually jump to a lower-tier provider if the gateway still has eligible higher-tier routes.

## 4. Browser-backed route fails

Inspect:

- account state;
- browser account runtime;
- browser session;
- Chromium driver status;
- adapter/page compatibility;
- authentication state.

Keep these failure classes separate:

- login/auth expired;
- provider page drift/adapter incompatible;
- browser/CDP transport failure;
- browserless/direct transport failure;
- provider rate/quota limit;
- provider-side model selection conflict.

CAPTCHA, 2FA and passkey steps require the user.

## 5. Request failed after routing

Use execution trace:

- `GET /_llmgateway/executions`
- `GET /_llmgateway/executions/{request_id}`

Report the selected route, fallback attempts and classified failure when the trace supports them.

Do not claim a fallback happened from the final model name alone.

## 6. Safe recovery order

Prefer:

1. retry a retryable transient failure through normal gateway fallback;
2. refresh model discovery when catalog staleness is evidenced;
3. re-verify/re-authenticate only when auth evidence requires it;
4. restart browser runtime only when runtime evidence requires it;
5. enable/disable/change groups only when the user explicitly wants a routing-state change.

Deletion is not a diagnostic step.
