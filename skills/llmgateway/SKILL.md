---
name: llmgateway
description: Use, inspect, route through, and diagnose a local llmgateway instance safely. Use when an AI agent needs model discovery, OpenAI/Anthropic-compatible inference, model-group routing, route diagnostics, client-policy awareness, or provider/account troubleshooting through llmgateway.
---

# llmgateway Agent Skill

Use llmgateway as the model runtime. Do not duplicate its routing logic in the agent.

## Core rule

Prefer this order:

1. discover the models visible to the current client;
2. prefer a logical/model-group ID when one matches the task;
3. let llmgateway apply readiness, policy, quota, priority tiers, health, recovery, fairness and fallback;
4. request a physical/provider model only when the user or task requires that exact model.

Never hard-code a provider model merely because it worked in a previous session.

## Connection

Default base URL:

`http://127.0.0.1:7331`

Override with `LLMGATEWAY_BASE_URL`.

For ordinary execution, prefer a scoped client key in `LLMGATEWAY_CLIENT_API_KEY`. If it is absent, the helper may use `LLMGATEWAY_API_KEY`.

Use the global `LLMGATEWAY_API_KEY` only for admin diagnostics and operations.

Never print, echo, serialize, commit, or return credential values.

## First-use workflow

1. Run health:
   `python3 skills/llmgateway/scripts/llmgateway_agent.py health`
2. Discover client-visible capability/model metadata:
   `python3 skills/llmgateway/scripts/llmgateway_agent.py capabilities`
3. Resolve semantic requirements through the gateway Router:
   `python3 skills/llmgateway/scripts/llmgateway_agent.py resolve --model llmgateway-auto --capability coding --capability reasoning --prompt "task summary"`
4. Execute with the **same requirements**, for example:
   `python3 skills/llmgateway/scripts/llmgateway_agent.py responses llmgateway-auto "task" --capability coding --capability reasoning`
5. If resolution is blocked, use client-scoped diagnostics:
   `python3 skills/llmgateway/scripts/llmgateway_agent.py diagnostics --model llmgateway-auto --capability coding`
6. Use admin route explain/account/browser diagnostics only when deeper operator evidence is needed.

Read [references/routing.md](references/routing.md) before making model-selection decisions.
Read [references/api.md](references/api.md) for endpoint details.
Read [references/mcp.md](references/mcp.md) when connecting an MCP host.

## Protocol choice

- Prefer **OpenAI Responses** for Codex-style or tool-oriented OpenAI clients.
- Use **OpenAI Chat Completions** for conventional chat clients.
- Use **Anthropic Messages** for Claude/Anthropic-compatible clients.
- Use persistent Threads only when the caller explicitly wants gateway-owned durable conversation state.

Do not invent endpoints. If the installed gateway differs from this skill, inspect its current docs or API surface before acting.

## Diagnostics workflow

When a request fails, do not immediately switch providers.

Follow this sequence:

1. health;
2. client-visible capability/model metadata;
3. client-scoped Agent diagnostics for the same requirements;
4. route explain only when deeper admin evidence is needed;
5. account/model/group state;
6. account intelligence and execution trace;
7. provider/browser diagnostics when the selected route is browser-backed;
8. retry only when the failure is classified as retryable.

Read [references/diagnostics.md](references/diagnostics.md) for the full decision tree.

## Safety boundary

The bundled helper intentionally exposes only READ and EXECUTE operations.

Permission levels:

- **READ**: health, model discovery, groups, accounts, clients, route explain, execution diagnostics.
- **EXECUTE**: inference requests.
- **OPERATE**: enable/disable account/model/group, refresh models, restart/stop browser runtime.
- **ADMIN**: account deletion, credential/config changes, destructive state changes.

READ and EXECUTE are normal skill operations.

Do not perform OPERATE actions unless the user's request clearly requires the state change. Describe the intended mutation before performing it.

Do not perform ADMIN/destructive actions without explicit user approval for that action. Never infer approval from a generic request to "fix" or "diagnose".

Read [references/operations.md](references/operations.md) before mutating gateway state.

## Browser/provider rules

- Browser auth is user-owned and interactive.
- CAPTCHA, 2FA and passkeys stay interactive.
- Never request or expose raw cookies, local storage tokens, refresh tokens or browser-profile secrets.
- Browserless/direct transport is an optimization over a valid authenticated session, not an auth bypass.
- Provider page drift, auth expiry, quota and route health are distinct failure classes. Preserve that distinction in reports.

## Multimodal feature detection

Do not assume file, vision, voice or image-generation APIs exist merely because another branch or future release has them.

Use the currently installed gateway's model/capability metadata and documented endpoints. If a required modality is not advertised, report it as unavailable instead of fabricating a request shape.

## Completion checks

Before reporting success:

- the selected model is visible to the caller;
- the request actually completed through llmgateway;
- any fallback claim is supported by route/execution evidence;
- no secret value is present in logs/output;
- no destructive state was changed unless explicitly authorized.
