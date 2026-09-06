# Agent-Native llmgateway

Tracking: #90.

## Goal

Make llmgateway a model runtime that AI agents can use without learning provider-specific plumbing.

The first slice is intentionally a portable Agent Skill built on the existing API surface.

```text
Agent intent
    |
    v
llmgateway skill
    |
    +-- discover client-visible models
    +-- choose logical model/group
    +-- execute through compatible API
    +-- inspect route/health evidence when needed
    |
    v
existing llmgateway router
    |
    +-- policy
    +-- readiness
    +-- ordered fallback tiers
    +-- quota/cooldown
    +-- task fit
    +-- adaptive health
    +-- browser/API policy
    +-- fairness/recovery
    |
    v
provider route
```

## Why the skill is thin

llmgateway already owns routing and provider state. A thick agent-side planner would create two competing routing systems and eventually drift.

The skill therefore teaches agents how to ask the gateway for the right abstractions and how to interpret diagnostics. It does not hard-code "best model" tables.

## Deliverables in P0

- `skills/llmgateway/SKILL.md`;
- progressive-disclosure references;
- stdlib-only read/execute helper CLI;
- deterministic offline helper tests;
- installation/usage documentation;
- explicit READ / EXECUTE / OPERATE / ADMIN boundary.

## Future slices

### P1 - Agent Control API

Add compact, stable agent-facing capability/status views only where the existing admin API is too verbose. Reuse the same Router and state stores.

Candidate concepts:

- capability summary;
- normalized diagnostic snapshot;
- safe probe endpoint.

Do not expose secrets or create a second routing engine.

### P2 - MCP server

Expose selected llmgateway operations as MCP tools with narrow schemas and permission-aware separation between read, execute and mutation actions.

### P3 - Capability-based agent routing

Let agents express requirements such as coding, reasoning, vision or context needs without provider-brand coupling, while the gateway resolves eligible logical/physical routes.

This should extend existing capability metadata and Router behavior rather than bypass model groups/client policies.

## Non-goals

- automated credential extraction;
- CAPTCHA/2FA bypass;
- unrestricted autonomous admin;
- provider-specific model rankings embedded in the skill;
- claiming unmerged multimodal APIs as shipped.
