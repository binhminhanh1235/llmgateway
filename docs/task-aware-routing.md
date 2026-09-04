# Task-aware routing

v0.26 adds deterministic workload classification to the existing route planner. The classifier runs locally and does not call another LLM.

The router still applies hard eligibility rules first:

1. route enabled
2. account/browser readiness
3. known context-window fit
4. route cooldown
5. quota eligibility

Eligible routes are then ranked with the same explainable score used at runtime:

```text
final_score = base_priority + quota_penalty + adaptive_penalty + task_adjustment
```

Lower scores win.

## Workload classes

The classifier exposes one primary `task.kind` plus boolean workload traits:

- `coding`
- `reasoning`
- `long_context`
- `simple_chat`
- `general`

Signals are intentionally conservative. Examples include code fences, programming/debugging terms, reasoning language, request size, requested output size, tool presence, and message count.

Long-context classification uses:

```text
required_context_tokens =
    estimated_input_tokens + requested_output_tokens
```

The token estimate is deliberately cheap and approximate. It is used for routing preference, not billing.

## Route metadata

Task-aware routing reuses route-level `capabilities`. These tags are policy hints, not provider claims:

| Tag | Effect |
| --- | --- |
| `coding`, `code`, `developer` | Prefer for coding workloads |
| `reasoning`, `deep-reasoning` | Prefer for reasoning workloads |
| `long-context`, `large-context` | Prefer for long-context workloads |
| `cheap`, `low-cost`, `fast`, `simple-chat` | Prefer for simple chat |
| `premium`, `expensive` | Small penalty for simple chat |

Example:

```toml
[[routes]]
id = "coder"
account = "qwen-primary"
model = "qwen3-coder-plus"
priority = 20
enabled = true
capabilities = ["chat", "tools", "coding", "reasoning"]

[[routes]]
id = "flash"
account = "gemini-primary"
model = "gemini-flash"
priority = 10
enabled = true
capabilities = ["chat", "cheap", "fast"]
```

A task-fit bonus is bounded by `routing.task_fit_max_bonus`. Missing task tags remain neutral, so old configurations do not suddenly become penalized.

## Context-window safety

A route may optionally declare a known total context window:

```toml
[[routes]]
id = "large-context"
account = "provider-account"
model = "model-id"
priority = 30
enabled = true
capabilities = ["chat", "long-context"]
context_window = 131072
```

When `context_window` is known and the request cannot fit, the route is excluded with:

```text
context_window_too_small
```

Unknown context windows remain eligible. This avoids pretending the gateway knows provider metadata it has not verified.

Discovered physical-model routes inherit catalog context-window and capability metadata when available.

## Configuration

Default controls:

```toml
[routing]
task_aware_enabled = true
task_fit_max_bonus = 20
task_mismatch_penalty = 12
task_long_context_threshold_tokens = 12000
task_simple_max_input_tokens = 800
```

The task adjustment is bounded. It cannot grow without limit and it does not bypass readiness, quota, or cooldown controls.

## Explicit task hint

Automatic classification is the default. A client that already knows the workload can send:

```json
{
  "model": "llmgateway-auto",
  "llmgateway_task": "coding",
  "messages": [
    {"role": "user", "content": "Implement the retry policy"}
  ]
}
```

Accepted values include:

- `coding`
- `reasoning`
- `long_context`
- `simple_chat`
- `general`
- `auto`

`llmgateway_task` is a gateway-only extension. It is removed before the request is sent upstream.

## Explain routing

The existing explain API can simulate a real request:

```bash
curl -X POST http://127.0.0.1:7331/_llmgateway/routes/explain \
  -H "Authorization: Bearer $LLMGATEWAY_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "llmgateway-auto",
    "body": {
      "messages": [
        {"role": "user", "content": "Debug this Rust compiler error"}
      ]
    }
  }'
```

Or force a workload class while debugging policy:

```json
{
  "model": "llmgateway-auto",
  "task": "reasoning"
}
```

The response now includes:

- `task.kind`
- estimated input/output/context tokens
- classifier signals
- per-route `task_adjustment`
- matched capability tags
- known `context_window`
- `context_sufficient`
- normal readiness/quota/adaptive fields
- final score and rank

This keeps task-aware routing observable instead of turning it into a black box.
