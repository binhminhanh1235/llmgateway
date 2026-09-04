# llmgateway roadmap

This roadmap tracks the path from the current v0.31 browser-first beta to a stable v1.0.

## Product direction

llmgateway is **browser-first**.

The primary execution lane should be authenticated browser accounts such as Gemini Web and Qwen Web. API-key accounts remain supported, but for the main local workflow they should behave as optional paid/credentialed fallbacks rather than the default path.

North-star experience:

```text
start llmgateway
      |
      v
Accounts UI
      |
      +-- Gemini account A ---- signed in
      +-- Gemini account B ---- signed in
      +-- Qwen account A ------ signed in
      |
      v
automatic readiness + routing + failover
      |
      +-- coding / reasoning / long context / simple chat
      |
      v
best eligible browser account/model
      |
      +-- optional API fallback only when needed
```

The security boundary stays unchanged:

- cookies, local storage, refresh state, CAPTCHA and 2FA remain inside isolated Chromium profiles;
- llmgateway never exposes raw browser credentials through its APIs;
- CAPTCHA/2FA and normal authentication are completed interactively by the user;
- browser integrations must respect provider terms, anti-abuse controls and quota limits.

## Current baseline: v0.31

Already implemented:

- OpenAI Chat Completions, OpenAI Responses and Anthropic Messages compatibility;
- persistent conversations, Structured Memory IR, compaction and retrieval;
- model catalog, aliases and virtual models;
- multi-provider / multi-account routing;
- sticky affinity and failover;
- unified account readiness;
- persistent quota/usage state;
- adaptive latency/reliability scoring with recovery;
- task-aware routing for coding, reasoning, long-context, simple chat and general workloads;
- route explain, execution trace and Trace Console;
- isolated browser profiles;
- Chromium launch/verify/stop lifecycle;
- CDP transport;
- first-class `browser-*` accounts without dummy API keys;
- browser readiness integrated into the same router as API accounts;
- browser auth failure transitions to `requires_attention`;
- Accounts UI browser controls;
- Docker and E2E regression coverage;
- startup and periodic browser runtime reconciliation;
- reconnect to Chromium that survives a gateway restart;
- stale CDP detection and safe profile relaunch after an unexpected browser crash;
- explicit browser lifecycle states and browser-specific Account Readiness reasons;
- browser-first virtual-model routing with configurable API fallback;
- live CDP readiness checks before browser-CDP routes are selected;
- versioned browser adapter contract v1;
- first-class `browser-gemini` and `browser-qwen` providers;
- embedded Gemini/Qwen adapter scripts with selector fallbacks;
- adapter health/version/page-signature diagnostics;
- explicit provider-page drift detection;
- configurable route-model to provider-UI model labels;
- stateless per-request provider tabs using the authenticated profile;
- prompt-mediated OpenAI tool-call bridge for coding-agent clients;
- deterministic fake-page and fake-CDP adapter fixtures;
- managed Browser Accounts wizard for Gemini Web and Qwen Web;
- safe linked config creation with validation, duplicate protection, backup, and atomic replacement;
- hot activation of browser sessions, Chromium sessions, provider bindings, accounts, routes, and catalog models;
- immutable request-level config snapshots across routing/execution during hot reload;
- browser account disable/re-enable, re-authentication, restart, stop, and recovery controls;
- deterministic browser-account hot-activation E2E coverage;
- true incremental CDP streaming for built-in Gemini/Qwen providers;
- downstream-driven browser stream polling and bounded in-page stream state;
- client-disconnect cancellation with provider Stop + ephemeral target cleanup;
- configurable first-byte and idle-stream timeouts;
- partial/cancelled stream metadata in Execution Trace and Trace Console;
- deterministic OpenAI Chat, Responses and Anthropic browser-stream compatibility E2E;
- least-recently-used fairness among equal-quality browser accounts;
- session-aware sticky browser affinity for persistent threads;
- bounded post-cooldown browser recovery scoring;
- explicit prefer-browser / browser-only / balanced / prefer-api / api-only policies;
- Model Catalog enrichment of configured route capability/context metadata;
- browser-aware route-explain policy, fairness and recovery diagnostics;
- deterministic multi-browser routing/fallback E2E coverage.

The remaining browser work is now mostly **client policy, usage/cost intelligence, and production hardening**, rather than browser execution plumbing.

---

## v0.27 - Browser Account Reliability ✅

**Status: shipped**

Browser accounts now survive ordinary local runtime failures and recover predictably without hand-holding. The implementation includes startup/background reconciliation, browser-first routing, live CDP readiness, crash recovery, stale-port cleanup, deliberate-stop semantics, and restart/recovery E2E coverage.

Original scope and acceptance criteria are retained below as the release contract.

### Scope

- make browser sessions a first-class startup lifecycle rather than a loosely coupled optional driver;
- auto-reconcile persisted browser session state with the actual Chromium process at startup;
- detect dead/stale Chromium processes and stale DevTools ports;
- reconnect to an existing healthy isolated profile when possible;
- auto-verify a launched/reconnected session before making its routes eligible;
- make lifecycle transitions explicit and deterministic:
  - `stopped`
  - `starting`
  - `login_required`
  - `ready`
  - `degraded`
  - `requires_attention`
  - `failed`;
- classify browser execution failures into actionable categories instead of generic transport failures;
- ensure a gateway restart reuses existing authenticated profiles without requiring a new login when the provider session is still valid;
- improve browser-specific health/recovery telemetry in Account Intelligence and Trace Console;
- add browser-first routing preference with configurable API fallback;
- preserve current safety behavior for 401/403, 429, CAPTCHA and 2FA.

### Required E2E scenarios

- gateway restart with an existing authenticated profile;
- Chromium killed while llmgateway is running;
- stale `DevToolsActivePort`;
- browser session not logged in;
- browser auth expires during a request;
- one browser account fails and another browser account succeeds;
- all browser routes fail and an explicitly configured API fallback succeeds;
- recovered browser route becomes eligible again without restarting llmgateway.

### Exit criteria

A normal browser-account user should not need to inspect process IDs, DevTools ports or SQLite state to recover a session.

---

## v0.28 - Production-grade Browser Provider Adapters ✅

**Status: shipped**

Gemini Web and Qwen Web now have first-class provider kinds behind a versioned adapter contract. The release adds pre-route probes, drift diagnostics, model mapping, stateless provider tabs, coding-agent tool bridging, and deterministic fake-page/CDP regression fixtures.

Original scope and acceptance criteria are retained below as the release contract.

### Scope

- version the browser adapter contract;
- keep provider-specific behavior outside Router/Gateway core;
- productionize Gemini Web adapter;
- productionize Qwen Web adapter;
- normalize provider response/error semantics into the existing gateway contract;
- expose adapter health/version diagnostics;
- detect provider-page drift and surface an actionable `adapter_incompatible` state;
- support configured model selection per browser account;
- attach verified capabilities/context metadata when known;
- add deterministic fake-page/CDP fixtures so CI does not depend on live provider websites;
- document provider-specific limitations and recovery steps.

### Exit criteria

Provider UI changes should fail loudly and diagnostically, not silently produce malformed responses or infinite retries.

---

## v0.29 - Browser Accounts UX ✅

**Status: shipped**

**Priority: P0**

Goal: adding and maintaining browser accounts should be possible from the local UI without manually editing several TOML sections.

v0.29 ships the managed Gemini/Qwen wizard, validated config persistence, hot activation without process restart, lifecycle controls, request-pinned immutable config snapshots, immediate catalog/router visibility, and a deterministic fake-CDP E2E gate.

### Scope

- Add Browser Account wizard:
  1. choose provider;
  2. create isolated profile;
  3. launch Chromium;
  4. user signs in normally;
  5. verify authenticated page;
  6. choose/configure models;
  7. enable routes.
- one-click actions:
  - Launch
  - Verify
  - Re-authenticate
  - Restart
  - Stop
  - Disable account;
- clearly show:
  - provider
  - profile
  - session lifecycle
  - readiness reason
  - last successful use
  - last failure
  - route/model bindings
  - cooldown/quota state;
- persist user-managed browser account configuration safely;
- validate account/session/binding/driver configuration as one unit;
- prevent accidental profile-directory collisions between accounts;
- make API fallback optional in the same setup flow.

### Exit criteria

A new user can add multiple Gemini/Qwen browser accounts from the UI and start routing through them without hand-authoring TOML.

---

## v0.30 - True Browser Streaming and Cancellation ✅

**Status: shipped**

**Priority: P0**

Goal: browser-backed requests should feel like native streaming LLM APIs.

v0.30 removes the old full-answer buffering boundary for built-in Gemini/Qwen adapters. The page exposes additive stream start/poll/cancel primitives; Rust converts those events to an upstream SSE body lazily, so downstream reads control CDP polling and disconnects trigger cancellation/cleanup.

### Scope

- true incremental CDP/browser streaming;
- token/chunk forwarding without buffering the entire answer;
- downstream cancellation propagation;
- client disconnect handling;
- backpressure;
- first-byte and idle-stream timeouts;
- partial-response trace metadata;
- deterministic cleanup after cancellation/failure;
- compatibility tests for OpenAI Chat, Responses and Anthropic Messages streaming.

### Exit criteria

Claude Code, Codex and OpenCode receive browser-backed streaming with no gateway-side full-response buffering.

---

## v0.31 - Browser-aware Routing Intelligence ✅

**Status: shipped**

**Priority: P1**

Goal: make multiple browser accounts operate like a smart local pool.

v0.31 keeps all existing hard readiness/quota rules, then adds explicit transport policy, cautious browser recovery scoring, and browser-only least-recently-used fairness. Persistent threads preserve healthy browser affinity while non-sticky requests rotate equal-quality accounts.

### Shipped scope

- browser account rotation/fairness within equal-quality routes;
- session-aware sticky affinity, configurable with `browser_sticky_affinity`;
- browser-specific cooldown/recovery scoring with bounded penalties;
- task-aware capability profiles for browser routes through Model Catalog enrichment;
- known context-window enforcement using route/catalog metadata;
- existing account-level usage/quota heuristics retained ahead of fairness;
- `prefer-browser`, `browser-only`, `balanced`, `prefer-api`, and `api-only` policies;
- backward-compatible `browser-first` and `api-first` aliases;
- deterministic paid API fallback control through `api_fallback` and `browser-only`;
- route explain fields for normalized policy, fairness rank, recovery penalty and fallback reason;
- deterministic two-browser + API fallback smoke coverage.

Configuration:

```toml
[routing]
execution_preference = "prefer-browser"
api_fallback = true
browser_fairness_enabled = true
browser_sticky_affinity = true
browser_recovery_penalty = 8
browser_recovery_max_penalty = 40
```

### Exit criteria

Equal-quality browser accounts share traffic predictably, persistent conversations do not churn sessions unnecessarily, recently unstable browsers recover cautiously, and API fallback is explicit and explainable.

---

## v0.32 - Client Policies and Budgets

**Priority: P1**

- per-client API keys;
- allowed virtual/physical models;
- browser-only or API-fallback permissions per client;
- request/token budgets;
- routing strategy per client;
- policies for Claude Code, Codex, OpenCode, OmniVoiceStudio and other local tools.

---

## v0.33 - Usage, Cost and Savings Intelligence

**Priority: P1**

- normalized request/token accounting;
- estimated API cost when pricing metadata is known;
- browser-vs-API usage breakdown;
- estimated avoided API spend;
- account/model/provider dashboard;
- routing reason analytics;
- quota/cooldown history.

Browser usage should not be assigned fictitious token prices. Cost fields remain unknown unless a real monetary price is known.

---

## v0.34 - Model and Cost Intelligence

**Priority: P2**

Normalize model metadata:

```text
ModelProfile
  context_window
  capabilities
  tool_support
  vision
  coding/reasoning suitability
  latency observations
  input_price
  output_price
```

Then extend route scoring with expected API cost where applicable while keeping browser routes price-neutral unless a known monetary cost exists.

---

## v0.35 - Production Hardening and Distribution

**Priority: P2**

- rate limits and concurrency controls;
- graceful shutdown;
- SQLite backup/restore and migration hardening;
- Prometheus/metrics endpoint;
- structured operational logs;
- stronger secret/config validation;
- Windows/Linux/macOS release binaries;
- automated GitHub Releases;
- install/upgrade documentation;
- migration compatibility policy.

---

## v1.0 - Stable Universal LLM Gateway

v1.0 gate:

- browser accounts are the polished primary execution path;
- multiple browser accounts can be added and operated from the UI;
- browser profiles survive gateway restarts;
- expired sessions have a clear re-authentication flow;
- browser-to-browser failover is reliable;
- optional browser-to-API fallback is deterministic;
- Gemini Web and Qwen Web adapters have stable diagnostics and regression fixtures;
- browser streaming is truly incremental;
- Claude Code, Codex and OpenCode work through browser-backed routes;
- routing/execution decisions are observable;
- config/data migrations are documented and tested;
- all release gates are green.

## Priority order

```text
P0  Browser reliability
    -> provider adapters
    -> browser UX
    -> true streaming

P1  Browser-aware routing
    -> client policies
    -> usage/cost dashboard

P2  model/cost intelligence
    -> production hardening
    -> distribution

    -> v1.0
```

The roadmap intentionally places Cost-aware Routing after the browser-account milestones. For the primary local use case, making browser execution boringly reliable is more valuable than optimizing paid API routing first.
