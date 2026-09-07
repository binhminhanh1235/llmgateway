# Provider Runtime Fabric — Development Plan

Baseline main:
`f85e54b8741a8a184bdc84b142c8b770adee29c0`

Working branch:
`feat/provider-runtime-fabric`

Status:
`P0 + P1 + P2 + P3 + P4 + P5 DONE / VERIFIED — initiative remains NOT ON MAIN`

## 1. Goal

Build a resource-efficient, self-healing provider runtime that keeps llmgateway responsive when web providers throttle, reject one transport, lose auth state, drop streams, or crash browser/CDP state.

The initiative must improve all of these at once:

- resilience;
- seamless user experience;
- low RAM/CPU use;
- deterministic failover;
- transport isolation;
- safe streaming retry/fallback;
- browser invisibility during normal operation.

This is not a provider-specific patch set. The architecture must remain reusable for Gemini, ChatGPT, Qwen, DeepSeek, MiMo, and future browser-backed providers.

## 2. Product invariants

The following are hard requirements.

### 2.1 Invisible-first execution

Normal background requests MUST NOT automatically open a visible browser window.

Execution preference for a browser-backed account is:

```text
1. direct HTTP
2. headless browser fetch
3. headless browser UI automation
4. visible browser only after explicit user action
```

Visible browser launch is allowed only for human interaction such as:

- login;
- OAuth;
- CAPTCHA;
- 2FA;
- consent;
- explicit manual recovery.

A background request that needs visible interaction must surface a typed `HUMAN_ACTION_REQUIRED` / auth-readiness state and let routing continue to another healthy candidate when possible.

### 2.2 Browser is an escalation resource

Browser processes are STOPPED by default and launched on demand.

Gateway startup must not eagerly launch one browser per configured account.

The runtime should support:

- single-flight browser startup;
- bounded browser-process count;
- idle shutdown;
- LRU/resource-pressure eviction;
- reuse of an already-running compatible browser;
- optional adaptive warm lifetime;
- headless execution by default.

### 2.3 Logical routing is transport-neutral

Router selects a logical candidate:

```text
provider + account + model
```

The account runtime selects the transport.

A failure of:

```text
Qwen/account-A/direct-http
```

must not automatically make:

```text
Qwen/account-A/model-X
```

unroutable when browser-fetch or browser-UI remains healthy.

### 2.4 Failure ownership

A failure is handled by the lowest layer capable of recovering it.

Examples:

| Failure | Primary owner |
| --- | --- |
| CDP channel reset | Browser supervisor |
| page target closed | Browser supervisor |
| direct HTTP WAF rejection | transport breaker |
| expired/incomplete auth | auth runtime |
| DeepSeek conversation desync | conversation runtime |
| account overload | admission controller |
| model unavailable | logical router |
| account exhausted | logical router |
| provider exhausted | virtual-model failover |

Router must not need to parse provider-specific WAF strings or CDP internals.

### 2.5 Canonical conversation ownership

llmgateway local thread/messages are canonical.

Provider-native conversation state is an optimization/cache, not the only source of truth.

A persistent virtual-model thread must be able to migrate to another healthy provider/account when provider-native affinity becomes unusable, subject to normal capability and policy constraints.

### 2.6 Safe retry and streaming

Retries and fallbacks must respect replay safety.

Execution attempts distinguish at minimum:

- pre-submit;
- post-submit with no client-visible output;
- post-commit/client-visible partial output.

Silent cross-provider fallback is allowed only while replay is safe.

Once client-visible output has crossed the stream commit barrier, the gateway must not silently splice a second model/provider response into the same stream.

## 3. Target architecture

```text
                         API Request
                              |
                              v
                     Execution Planner
                  budget / deadline / policy
                              |
                              v
                      Logical Router
                 provider / account / model
                              |
                              v
                 Provider Account Runtime
              +-------------------------------+
              | admission controller          |
              | auth generation               |
              | conversation leases           |
              | resource health graph         |
              | transport strategy            |
              +---------------+---------------+
                              |
            +-----------------+-----------------+
            |                 |                 |
            v                 v                 v
       Direct HTTP      Headless Fetch     Headless UI
            |                 |                 |
            +-----------------+-----------------+
                              |
                       Browser Runtime
              +-------------------------------+
              | supervisor                    |
              | single-flight startup         |
              | browser/page generations      |
              | reconnect/reacquire/restart   |
              | resource budget + idle stop   |
              +---------------+---------------+
                              |
                              v
                        Stream Barrier
                              |
                              v
                            Client
```

## 4. Core runtime concepts

### 4.1 Typed failure taxonomy

Introduce a provider-neutral failure contract.

Candidate classes include:

- `AuthExpired`
- `AuthIncomplete`
- `HumanActionRequired`
- `RateLimited`
- `UpstreamOverloaded`
- `WafRejected`
- `CdpDisconnected`
- `PageTargetLost`
- `BrowserCrashed`
- `SessionBusy`
- `SessionStateDesync`
- `StreamDropped`
- `StreamEmpty`
- `ModelUnavailable`
- `ModelRecipeStale`
- `NetworkTransient`
- `Upstream5xx`

Failure metadata should include:

- provider;
- account;
- model;
- transport;
- conversation when applicable;
- retryability;
- replay safety;
- breaker scope;
- suggested cooldown;
- human-action requirement.

Avoid making Router depend on string matching.

### 4.2 Replay safety

Define:

```text
Safe
ProbablySafe
Unsafe
```

or an equivalent explicit contract.

The transport/provider layer must identify whether submission may already have happened.

### 4.3 Resource health graph

Track health at the smallest practical resource scope.

Example:

```text
Provider
  Account
    Auth generation
    Direct HTTP
    Browser generation
      CDP channel
      Page target
      Conversation
```

Failure on one node must not poison healthy siblings unnecessarily.

### 4.4 Circuit breaker

Support:

- CLOSED;
- OPEN;
- HALF_OPEN;
- bounded half-open probes;
- exponential cooldown;
- jitter;
- hysteresis before returning to full-health priority.

Some failures may be generation-bound rather than time-bound.

Example: repeated Qwen direct-HTTP WAF rejection can quarantine that transport for the current auth/runtime generation instead of probing every few seconds.

### 4.5 Account runtime / actor boundary

Each account runtime serializes lifecycle transitions and owns:

- admission;
- auth;
- transport selection;
- browser escalation;
- conversation leases;
- health mutation;
- cleanup.

It must prevent:

- duplicate browser startup;
- stop/start races;
- stale-generation callbacks;
- overlapping provider requests when the provider cannot safely handle them.

### 4.6 Execution budget

Every request carries bounded recovery limits, e.g.:

- overall deadline;
- max attempts;
- max transport switches;
- max account switches;
- max provider switches;
- max queue wait.

No retry/fallback loop may run unbounded.

## 5. Resource efficiency design

### 5.1 Default browser state

`STOPPED`.

### 5.2 Browser modes

Support user-facing resource profiles:

#### Efficient

- no prewarm;
- aggressive idle shutdown;
- low maximum browser count;
- direct-first;
- cold headless start when required.

#### Balanced

- short warm retention for recently used accounts;
- bounded browser count;
- adaptive idle shutdown.

#### Performance

- longer warm retention;
- optional prewarm for frequently used accounts;
- higher resource budget.

Default should remain conservative for personal machines.

### 5.3 Global browser budget

Plan for configuration such as:

```toml
[browser_runtime]
mode = "balanced"
allow_visible_auto_launch = false
max_running_browsers = 1
idle_timeout_secs = 45
prewarm = false
```

Exact names may change during implementation, but behavior must remain explicit.

### 5.4 Activation/resource cost in routing

Logical candidate scoring may consider execution activation cost when quality is otherwise close.

Example ordering:

```text
direct HTTP ready             cheapest
existing headless runtime     low cost
cold headless startup         higher cost
human interaction required    ineligible for background traffic
```

Resource cost must not override hard capability or correctness requirements.

### 5.5 Single-flight startup

Concurrent requests needing the same stopped browser runtime must await one startup operation rather than launching multiple Chromium instances.

### 5.6 Event-driven automation

Prefer:

- network events;
- CDP events;
- `ReadableStream`;
- `MutationObserver`;

over tight DOM polling loops.

## 6. Browser runtime design

### 6.1 Headless-first supervisor

Managed browser automation must use true headless execution during normal requests.

Do not implement "open visible then minimize" as a workaround.

### 6.2 Recovery ladder

On browser/CDP failure:

```text
reconnect channel
    -> reacquire target
    -> create replacement page
    -> restart browser
    -> quarantine profile/session
```

Do not restart the whole browser when a cheaper recovery layer can succeed.

### 6.3 Generation IDs

Browser/session/page runtime state should use generation/epoch identifiers so callbacks from dead browser/page instances cannot mutate current state.

### 6.4 CDP channel abstraction

Introduce a transport abstraction suitable for:

- WebSocket CDP;
- potentially remote-debugging pipe for gateway-managed Chromium.

Pipe support is optional until validated, but architecture must not hard-code WebSocket semantics into account/provider logic.

### 6.5 Browser fetch transport

Investigate and, where provider behavior permits, implement authenticated fetch from inside the headless browser origin/profile.

Conceptual priority:

```text
direct HTTP
  -> browser-context fetch
  -> browser DOM/UI automation
```

Browser-fetch must still respect provider terms, quota controls, authentication boundaries, and anti-abuse behavior.

It must not attempt to bypass or defeat WAF/CAPTCHA protections.

## 7. Auth runtime

Browser profile/authenticated interactive login is the durable source of auth truth.

Captured direct-HTTP auth material is a derived snapshot/cache.

Track auth generation with metadata such as:

- generation ID;
- captured time;
- required-cookie readiness;
- token readiness;
- expiry when known;
- source browser/profile revision.

A new successful login/auth refresh creates a new generation and invalidates stale auth-bound breaker state.

Missing auth requirements must be detected before sending normal requests where practical.

## 8. Provider-specific acceptance targets

### 8.1 Qwen

Current failure modes:

- Aliyun WAF rejection on direct HTTP;
- CDP WebSocket reset.

Required behavior:

- WAF rejection opens/quarantines only the direct transport;
- no rapid direct retry hammering;
- browser escalation remains available when healthy;
- browser automation is headless during normal traffic;
- CDP failure follows reconnect/reacquire/restart ladder;
- no visible window auto-opens;
- virtual models remain usable through alternate healthy candidates.

### 8.2 Gemini

Current failure modes:

- 503/rate-limit under bursts;
- auth snapshot/cookie readiness issues.

Required behavior:

- per-account admission control;
- bounded queueing;
- adaptive concurrency/rate response;
- typed throttled/cooldown state;
- strict auth-readiness before request;
- virtual model reroute while an account cools down;
- no visible browser launch for ordinary recovery.

### 8.3 DeepSeek

Current failure mode:

- intermittent empty stream under burst/shared token state.

Required behavior:

- account/conversation lease;
- lease remains held through terminal stream cleanup;
- conversation epoch/generation;
- empty/dropped stream marks conversation dirty;
- safe fresh-conversation retry before stream commit;
- late events from old epochs ignored;
- client cancellation cleans/quarantines state deterministically.

## 9. Development phases

### P0 — Execution Contract Foundation

Deliver:

- typed failures;
- replay-safety contract;
- execution phase tracking;
- recovery/execution budget;
- stream commit barrier contract;
- trace fields.

Acceptance:

- existing providers compile behind the new contract;
- no Router string parsing added;
- tests prove no silent fallback after committed partial output;
- existing compatibility APIs remain behaviorally compatible.

P0 verification — 2026-09-07:

- verified code head: `c37da161da864f63b8a1107e573bde1e1e0a4fe1`;
- full CI: run #1900 / `34071241430` — SUCCESS;
- Linux job `101588869977` — SUCCESS, including `cargo fmt --all -- --check`, `RUSTFLAGS="-D warnings" cargo check --all-targets`, `cargo clippy --all-targets`, `cargo test --all-targets`, routing/browser/streaming/execution-trace smoke suites, and Docker build;
- Windows job `101588869839` — SUCCESS, including PowerShell validation, `cargo check --all-targets`, `cargo test --all-targets`, and Chromium-driver Windows smoke;
- browser/provider compatibility classification is owned by `BrowserProviderError` / browser-provider boundary, while common `execution.rs` remains provider-neutral;
- Qwen `AdapterIncompatible(code=upstream_waf_rejected)` normalizes to `FailureClass::WafRejected`;
- CDP disconnect, page-target loss, browser crash, empty stream, and dropped stream normalize to typed common failures at the browser-provider boundary;
- production Gateway/Router/common execution paths do not parse provider-specific WAF/CDP/empty-stream/model-binding strings for retry/fallback decisions;
- legacy outward semantics remain preserved through the typed `GatewayError::Classified { failure, source }` compatibility wrapper, including `model_binding_conflict` HTTP 409, browser session/transport/adapter/model errors, and `model_recipe_stale`;
- regression coverage verifies safe pre-commit fallback, no silent fallback after client-visible commit, cancellation after commit, bounded execution budget, non-retry stale recipes, and non-retry/no-health-mutation model-binding conflicts.

**P0 is DONE / VERIFIED on the working branch only. The Provider Runtime Fabric initiative is still NOT ON MAIN and must not be described as shipped.**

### P1 — Runtime Health Graph & Breakers

Deliver:

- health keys/scopes for account/transport/session resources;
- CLOSED/OPEN/HALF_OPEN state;
- exponential cooldown + jitter;
- hysteresis;
- route eligibility integration without coupling Router to provider internals.

Acceptance:

- direct transport failure can be excluded while same logical account/model remains usable through another transport;
- half-open probe concurrency is bounded;
- recovery cannot flap immediately to top priority.

P1 verification — 2026-09-07:

- verified code head: `9b351c707ed99fc3f3383987dc4f68bcd2eb1164`;
- full CI: run #1908 / `34074083568` — SUCCESS;
- Linux job `101596735767` — SUCCESS, including `cargo fmt --all -- --check`, `RUSTFLAGS="-D warnings" cargo check --all-targets`, `cargo clippy --all-targets`, `cargo test --all-targets`, the full routing/browser/streaming/execution-trace smoke chain, and Docker build;
- Windows job `101596735884` — SUCCESS, including PowerShell validation, `cargo check --all-targets`, `cargo test --all-targets`, and Chromium-driver Windows smoke;
- shared provider-neutral `RuntimeHealthGraph` now tracks account/transport/session health with CLOSED/OPEN/HALF_OPEN breaker state;
- breaker recovery uses bounded HALF_OPEN concurrency, exponential cooldown with deterministic jitter, and two-success hysteresis before returning to CLOSED;
- browser-backed accounts isolate `direct_http` health from `browser_runtime`, so a broken direct transport does not automatically disable the same logical account/model while browser transport remains healthy;
- account-scoped failures such as 429 continue to preserve existing route-cooldown semantics, while transport/session/provider-scoped failures no longer unnecessarily lock the whole logical route;
- Router consumes only provider-neutral runtime-health snapshots and structured failure scopes; production Gateway/Router/runtime-health code does not contain provider-specific Qwen/Gemini/DeepSeek/WAF/CDP parsing;
- regression coverage locks transport isolation, bounded half-open probes, hysteresis, exponential cooldown+jitter, scoped cooldown compatibility, and route-explain compatibility.

**P1 is DONE / VERIFIED on the working branch only. P2 verification is recorded below. The Provider Runtime Fabric initiative remains NOT ON MAIN and must not be described as shipped.**

### P2 — Account Runtime & Admission Control

Deliver:

- per-account runtime ownership;
- single-flight lifecycle operations;
- bounded request queue;
- provider capability for concurrency/rate policy;
- adaptive concurrency baseline.

Acceptance:

- no duplicate browser startup under concurrent requests;
- overload is queued/rerouted instead of uncontrolled bursting;
- runtime shutdown is race-safe.

P2 verification — 2026-09-07:

- verified code head: `c4a663797c015183fa2e4f06f387b119794d212b`;
- full CI: #1915 / run `34078114968` — SUCCESS;
- Linux job `101608110226` — SUCCESS, including `cargo fmt --all -- --check`, `RUSTFLAGS="-D warnings" cargo check --all-targets`, `cargo clippy --all-targets`, `cargo test --all-targets`, the full routing/browser/streaming/execution smoke chain, and Docker build;
- Windows job `101608110289` — SUCCESS, including PowerShell validation, `cargo check --all-targets`, `cargo test --all-targets`, and Chromium-driver Windows smoke;
- provider-neutral `AccountRuntimeRegistry` now owns per-account admission, in-flight accounting, bounded queues, concurrency limits, adaptive pressure, lifecycle serialization, and structured runtime diagnostics;
- queue admission is bounded by both runtime policy and the request execution budget, with typed `AdmissionRejected`, `QueueOverflow`, and `QueueTimeout` execution failures for reroute/fail decisions;
- response/stream lifetime owns the admission permit, so cancellation and terminal stream completion release capacity without permit leaks;
- throttling/overload reduces effective concurrency immediately while recovery increases capacity only after repeated successes, with independent state per account;
- browser cold-start lifecycle is single-flight, concurrent failed waiters share one startup result, Chromium launch/stop is serialized per session, and generation invalidation prevents stale callbacks after reload/reset/manual lifecycle changes;
- browser-backed web accounts use a conservative serialized P2 baseline unless a future explicit provider capability proves higher safe concurrency;
- account enable/disable is reflected in shared runtime admission state, and `account-intelligence` / browser diagnostics expose queue depth, queue wait, in-flight count, admission state, effective concurrency limit, generation, and last rejection reason;
- deterministic tests cover concurrent cold start, failed-start single-flight, per-account limits, bounded queue/overflow, deadline-aware queue wait, adaptive reduction/recovery, account isolation, cancellation, aborted startup, stale generation, and shutdown/reload reuse;
- Router remains provider-neutral and P1 `RuntimeHealthGraph` remains the single breaker/health system rather than introducing a competing P2 health graph.

**P2 is DONE / VERIFIED on the working branch only. P3 verification is recorded below. The Provider Runtime Fabric initiative remains NOT ON MAIN and must not be described as shipped.**

### P3 — DeepSeek Deterministic Stream State

Deliver:

- account/conversation leases;
- epoch/generation checks;
- terminal cleanup;
- dirty-session quarantine;
- safe fresh-session retry before commit.

Acceptance:

- burst regression no longer reproduces empty-output race under deterministic fixture;
- cancellation leaves no leaked active lease;
- no retry after unsafe commit.

P3 verification — 2026-09-07:

- verified code head: `401adfc913813478b4f55403de6b7367a3aa553c`;
- full CI: #1921 / run `34080215157` — SUCCESS;
- Linux job `101613966406` — SUCCESS, including `cargo fmt --all -- --check`, `RUSTFLAGS="-D warnings" cargo check --all-targets`, `cargo clippy --all-targets`, `cargo test --all-targets`, the complete routing/browser/streaming/execution smoke chain, and Docker build;
- Windows job `101613966584` — SUCCESS, including PowerShell validation, `cargo check --all-targets`, `cargo test --all-targets`, and Chromium-driver Windows smoke;
- DeepSeek direct HTTP now owns a provider/account/thread conversation lease through terminal response cleanup; concurrent turns for the same conversation are serialized while unrelated conversations remain independent;
- each lease carries a monotonic conversation epoch; persisted schema v2 state records `conversation_epoch`, `dirty`, and `quarantined`, and stale epochs cannot overwrite a newer generation;
- dropped/empty logical completion is retryable only inside the DeepSeek adapter and only before any client-visible content/reasoning has been emitted; one retry creates a fresh DeepSeek session and advances the epoch, while post-commit failures never retry;
- terminal success persists a clean continuation state and releases the lease; terminal failure/cancellation quarantines the conversation before releasing the lease so a later request starts a fresh session instead of reusing uncertain state;
- deterministic coverage includes same-conversation burst serialization, cancellation lease release, epoch advance/recovery, no retry after commit, and an integrated 8-request burst fixture with exactly one simulated empty pre-commit recovery and no overlap;
- the P3 diff from the P2 docs head is confined to `src/deepseek_web_transport.rs`; Gateway/Router/common execution remain provider-neutral and P4 was not started.

**P3 is DONE / VERIFIED on the working branch only. P4 verification is recorded below. The Provider Runtime Fabric initiative remains NOT ON MAIN and must not be described as shipped.**

### P4 — Invisible Browser Runtime

Deliver:

- true headless normal execution;
- visible-browser hard policy;
- explicit user-triggered login/re-auth flow;
- resource budget;
- idle shutdown;
- LRU/resource eviction;
- reconnect/reacquire/restart ladder;
- browser/page generation IDs.

Acceptance:

- normal background request never opens visible browser;
- gateway start with configured browser accounts does not eagerly launch all Chromium instances;
- idle browser is reclaimed;
- concurrent cold starts launch one browser only;
- CDP reset can recover without unnecessary full restart where possible.

P4 verification — 2026-09-07:

- verified code head: `2f0ad33dac21c963df999247a3af4c018ce26e46`;
- exact code-head tree: `72750b7d0979bfd2709b4d4175da7945f04fb0c8`;
- full CI: #1941 / run `34087147618` — SUCCESS;
- Linux job `101633248671` — SUCCESS, including `cargo fmt --all -- --check`, `RUSTFLAGS="-D warnings" cargo check --all-targets`, `cargo clippy --all-targets`, `cargo test --all-targets`, the complete provider/browser/routing/execution smoke chain, P4 invisible-runtime and CDP reliability acceptance, and Docker build;
- Windows job `101633248569` — SUCCESS, including PowerShell validation, `cargo check --all-targets`, `cargo test --all-targets`, and Chromium-driver Windows smoke;
- managed browser accounts are cold by default; gateway startup does not eagerly launch Chromium for configured accounts, while ordinary execution acquires an on-demand background lease and launches true headless Chromium;
- automatic visible launch is hard-disabled for background traffic; explicit login/re-authentication remains interactive, and successful verify captures auth then closes the visible browser before normal work resumes;
- per-session lifecycle is single-flight and generation-safe, concurrent cold requests share one startup, stream/cancellation lifetime holds the browser lease, and stale callbacks cannot mutate a newer runtime generation;
- runtime resource controls enforce bounded running-browser count, idle/TTL reclaim and least-recently-used eviction without stopping a browser that still has an active execution lease;
- CDP readiness/recovery reuses or reacquires a healthy target before escalating to a managed headless restart, and browser stream error sources survive the lease wrapper so downstream diagnostics retain the provider-boundary cause;
- legacy non-CDP `browser-http` bridges remain outside Chromium lifecycle ownership, preserving the pre-P4 compatibility boundary;
- deterministic smoke coverage verifies invisible post-login closure, headless cold start, conversation affinity, ChatGPT recovery, browser streaming/cancellation, account UX, CDP reliability/restart reuse, routing intelligence and the complete regression chain.

**P4 and P5 are DONE / VERIFIED on the working branch only. Later P6-P8 status is recorded in the phase sections below. The Provider Runtime Fabric initiative remains NOT ON MAIN and must not be described as shipped.**

### P5 — Browser Fetch Transport

**Status: DONE / VERIFIED on `feat/provider-runtime-fabric` only.**

Verified code head:

- commit `46e979644e89c09157c2732511a00c9fe0cda078`;
- tree `190c5ee173f318cdd3670bc44a6d87004948fa2e`;
- CI #1948 / run `34096872023` — SUCCESS;
- Linux job `101662375329` — SUCCESS, including adapter fixtures, Rust fmt/check/clippy/tests, complete P0-P4 provider/browser/routing/execution regression chain, P5 fake-CDP acceptance and Docker;
- Windows job `101662374960` — SUCCESS, including PowerShell validation, `cargo check --all-targets`, `cargo test --all-targets` and Chromium-driver Windows smoke.

Delivered:

- provider-neutral browser-fetch capability and execution operation at the browser-provider boundary;
- physical transport order for browserless-capable accounts is `direct_http -> browser_fetch -> browser_runtime`, while explicit `browser-only` preserves UI/CDP-only compatibility;
- P4 `BrowserRuntime` lifecycle/lease and existing CDP target selection/recovery are reused; P5 does not create a second browser stack;
- `direct_http`, `browser_fetch` and `browser_runtime` use distinct runtime-health resource keys, so one transport failure does not automatically poison another;
- browser-fetch failures are classified at the adapter/provider boundary, with silent fallback to headless UI allowed only before client-visible commit when replay safety permits;
- incremental browser-fetch streaming reuses the existing CDP stream start/poll/cancel and stream commit/replay-safety machinery, including cancellation propagation and no whole-response buffering for a real incremental stream;
- Qwen browser-context fetch executes from the authenticated `chat.qwen.ai` page/origin, uses provider-owned browser cookies/session state, supports buffered + SSE delivery, and does not synthesize or bypass Aliyun WAF/CAPTCHA/anti-abuse material;
- Qwen challenged/rejected/unsupported browser-fetch paths become typed failures and may fall back safely to headless UI before commit;
- Gemini browser-context fetch executes from authenticated `gemini.google.com` context only where P5 can preserve verified semantics; current P5 feasibility is intentionally conservative for fresh/default-model text requests;
- Gemini selected-model private recipes and provider-native conversation continuation are typed as browser-fetch unsupported and fall back to the existing headless UI adapter rather than silently changing model/thread semantics;
- tool-call or unsupported multimodal shapes are rejected before browser-fetch submission when exact semantics are not preserved;
- existing outward errors and P0-P4 behavior remain intact, including `BrowserSessionUnavailable`, `BrowserTransport`, `BrowserAdapterIncompatible`, `BrowserModelUnavailable`, `model_binding_conflict` HTTP 409, `model_recipe_stale`, cancellation and invisible/headless policy.

Deterministic acceptance covers:

- fake-CDP browser-fetch buffered success;
- fake-CDP incremental stream success;
- cancellation/Abort propagation;
- direct HTTP failure path reaching browser-fetch before UI;
- browser-fetch pre-commit typed fallback to headless UI;
- browser-fetch vs headless-UI health isolation;
- committed/unsafe stream failures blocking silent fallback;
- Qwen/Gemini adapter ownership and semantic-loss guards;
- no DOM/native-conversation mutation during browser-fetch;
- no automatic visible Chromium launch;
- legacy UI streaming ephemeral cleanup under explicit `browser-only`;
- complete P0-P4 regression chain, Linux + Windows and Docker.

No provider anti-abuse control is bypassed by P5.

### P6 — Auth Generations & Gemini Hardening

**Status: DONE / VERIFIED on `feat/provider-runtime-fabric` only.**

Verified code head:

- commit `7a4592385141b3ad046f376995878813495c71a8`;
- tree `e83f96fe4311849b78b036b1fea4a46719c5de56`;
- CI #1954 / run `34104368716` — SUCCESS;
- Linux job `101685910340` — SUCCESS;
- Windows job `101685910051` — SUCCESS.

Delivered and verified:

- browser auth snapshots use explicit generations/fingerprints rather than mutable implicit cookie state;
- only a verified re-authentication replaces the current valid auth generation; incomplete/failed re-auth cannot clobber a known-good generation;
- required auth material is validated before normal direct execution where the provider contract permits it;
- stale auth-bound state is invalidated by generation changes and late work cannot silently mutate a newer generation;
- direct auth failure has bounded generation-aware recovery rather than an unbounded retry loop;
- Gemini distinguishes auth-incomplete/expired conditions from provider overload/rate-limit responses;
- Gemini 429/503 behavior feeds account-scoped admission/cooldown semantics instead of browser-session churn;
- the conservative web-provider admission policy serializes unsafe shared-session concurrency and uses bounded queueing;
- deterministic 40-request stress coverage proves overload is bounded by account admission instead of blindly bursting upstream.

Acceptance:

- missing auth is rejected before ordinary upstream execution where practical — VERIFIED;
- 40-request stress is bounded by admission control — VERIFIED;
- throttling/cooldown remains provider-neutral to Router and permits alternate logical candidates — VERIFIED by P6/P7 regression coverage.

### P7 — Logical Route / Transport Separation & Virtual-Model Continuity

**Status: DONE / VERIFIED on `feat/provider-runtime-fabric` only.**

Verified code head:

- commit `1f78c424501ae90c8fb817ee3b99ddc88359b16b`;
- tree `c36ecd4102094692ff6214b0620c739e31338059`;
- CI #2004 / run `34120630383` — SUCCESS;
- Linux job `101737725328` — SUCCESS, including full Rust checks/tests, provider/browser/routing/execution smoke chain and Docker;
- Windows job `101737725090` — SUCCESS, including `cargo check --all-targets`, `cargo test --all-targets` and Chromium-driver Windows smoke.

Delivered and verified:

- Router selects a logical provider/account/model candidate and stays provider-neutral;
- `AccountRuntimeRegistry` owns the ordered physical transport plan for browser-backed accounts;
- normal browserless-capable order is `direct_http -> browser_fetch -> browser_runtime`, while explicit `browser-only` keeps its compatibility boundary;
- logical scoring includes structured activation cost/reason and distinguishes direct-ready, warm headless and cold browser activation;
- route explain exposes activation cost/reason plus direct/warm readiness without leaking auth material;
- transport health remains isolated, so a failed physical transport does not automatically poison the logical account/model;
- canonical local conversation history remains the source of truth across provider/account switches;
- provider-native affinity remains an optimization and the gateway reconstructs context from the persistent local thread when failover crosses providers;
- deterministic failover verifies a Gemini-backed local thread can continue through the API fallback with prior local history preserved;
- `llmgateway-best` continuity is verified with Gemini as the preferred eligible browser candidate and API fallback after Gemini is disabled;
- `llmgateway-coding` continuity is verified with Qwen as the preferred coding-capable browser candidate and API fallback after Qwen is disabled;
- when every logical candidate is unavailable, route planning terminates with no selected route rather than entering a transport-recovery loop.

Acceptance:

- `llmgateway-best` and `llmgateway-coding` survive preferred-provider failure when an alternate eligible candidate exists — VERIFIED;
- transport failure alone does not incorrectly disable a logical model candidate — VERIFIED;
- persistent local threads continue across alternate provider/account selection using canonical local context — VERIFIED.

### P8 — Chaos, Resource & Live Acceptance

**Status: deterministic/resource implementation DONE / VERIFIED; authenticated live gate READY / PENDING.**

Deterministic/resource code head is the P7 verified head:

- commit `1f78c424501ae90c8fb817ee3b99ddc88359b16b`;
- tree `c36ecd4102094692ff6214b0620c739e31338059`;
- CI #2004 / run `34120630383` — SUCCESS on Linux + Windows.

Deterministic coverage includes:

- Qwen WAF/direct-transport rejection and adapter-owned typed classification;
- Gemini 429/503 burst handling and missing/incomplete auth;
- direct transport unavailable with bounded browser-fetch/headless escalation;
- CDP channel reset, page-target loss and browser restart/reacquire recovery;
- DeepSeek empty/dropped stream recovery with conversation epochs and dirty-state quarantine;
- partial committed streams blocking unsafe silent fallback;
- client cancellation and terminal lease/admission cleanup;
- concurrent cold browser startup with single-flight lifecycle;
- idle/TTL/LRU browser reclaim and active-lease protection;
- all primary logical candidates unavailable;
- canonical provider/account failover and built-in virtual-model continuity.

Resource/diagnostic instrumentation now exposes provider-neutral:

- running browser process count;
- active browser lease count;
- background launch count;
- automatic reclaim count;
- account in-flight count;
- queue depth/max queue depth;
- effective concurrency limit;
- runtime/auth generations;
- transport activation cost/reason.

Acceptance runners:

- `scripts/smoke-provider-runtime-fabric.sh` composes deterministic fault/resource suites into one local gate;
- `scripts/live-deepseek-provider-runtime.sh` provides conservative authenticated DeepSeek acceptance;
- `scripts/live-provider-runtime-fabric.sh` composes authenticated Gemini, Qwen and DeepSeek acceptance and verifies no leaked browser processes, browser leases, account in-flight work or queued work at completion;
- all new live runners are syntax-validated in CI and intentionally do not bypass CAPTCHA, WAF, provider throttling or other anti-abuse controls.

The final authenticated live run requires real local provider sessions and therefore is **not claimed as executed by CI**. Until that gate passes with real Gemini/Qwen/DeepSeek accounts, P8 is not final-live VERIFIED and the overall initiative must not be described as fully DONE / VERIFIED or shipped.

Live acceptance must use authenticated provider sessions only where required and must never treat provider anti-abuse controls as something to bypass.

## 10. UX requirements

Accounts UI should expose runtime state without leaking credentials.

Useful states:

- Ready;
- Direct;
- Headless fallback;
- Throttled;
- Cooling down;
- Needs login;
- Human action required;
- Browser stopped;
- Browser active;
- Browser recovering.

When an account needs login:

```text
Gemini needs re-authentication.
Requests can continue through configured fallback providers.

[Re-authenticate]
```

The visible browser opens only after that explicit action.

Resource settings should expose simple profiles rather than requiring users to understand Chromium internals.

## 11. Observability

Extend execution trace/runtime diagnostics with structured fields such as:

- logical candidate;
- selected transport;
- transport switch reason;
- account queue wait;
- in-flight count;
- breaker state;
- health scope;
- auth generation;
- browser generation;
- page generation;
- replay safety;
- commit state;
- fallback count;
- browser activation cost;
- browser launch/reuse/idle-stop reason.

Diagnostics must never expose raw cookies/tokens.

## 12. Compatibility and migration

Requirements:

- existing config remains valid;
- existing `browser-only`, `http-preferred`, routing policies and account toggles receive documented compatibility mapping;
- no automatic visible-browser behavior is introduced as a compatibility fallback;
- roll out new runtime incrementally behind compatible internal boundaries;
- avoid a flag day rewrite of all provider adapters.

## 13. Non-goals

This initiative must not:

- defeat WAF/CAPTCHA/anti-abuse mechanisms;
- spoof or rotate identities to evade provider controls;
- silently increase paid API usage beyond existing policy boundaries;
- merge unrelated multimodal work;
- require remote browser infrastructure;
- make headless/browser execution mandatory for API-only installations.

Remote browser worker support may be designed as a future-compatible interface, but is optional for this initiative.

## 14. Definition of Done

The initiative is DONE / VERIFIED only when all are true:

1. normal background traffic never auto-opens a visible browser;
2. browsers are stopped by default and reclaimed after idle/resource pressure;
3. one transport failure does not unnecessarily poison a logical account/model;
4. Qwen WAF direct failure does not cause rapid retry loops;
5. Qwen CDP recovery uses layered recovery;
6. Gemini bursts are admission-controlled and throttling-aware;
7. DeepSeek stream/conversation state is concurrency-safe;
8. virtual models survive a preferred-provider failure when another eligible candidate exists;
9. unsafe partial streams are never silently stitched to a fallback response;
10. execution/recovery loops are bounded by budget/deadline;
11. deterministic chaos/resource tests pass;
12. live authenticated acceptance validates the intended Qwen/Gemini/DeepSeek behaviors;
13. documentation, route explain, diagnostics, and UI reflect runtime state accurately;
14. no merge to `main` occurs until explicit approval after final verification.

## 15. Initial implementation rule

Start with P0 only.

Do not begin provider-specific fixes until the common execution/failure/replay contract is implemented and covered by tests.

Do not merge this branch into `main` without explicit approval.
