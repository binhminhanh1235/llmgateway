# Native MCP server

Tracking: issue #92 / PR #93.

## Single-executable invariant

MCP is part of the Rust `llmgateway` executable.

Production/runtime requirements do **not** include:

- Python;
- pip;
- Node.js;
- npm;
- a separately installed MCP bridge.

The old Python proof-of-concept bridge is removed before this work is merged.

## HTTP MCP

Start the normal gateway:

```bash
llmgateway
```

Endpoint:

```text
POST http://127.0.0.1:7331/mcp
```

The current native HTTP path targets MCP `2026-07-28`.

Modern requests are stateless and validate:

- `MCP-Protocol-Version`;
- `Mcp-Method`;
- `Mcp-Name` for tool calls;
- normal llmgateway client authentication.

MCP does not create a second authorization model. The same ClientPolicy boundary remains authoritative.

## STDIO compatibility

Hosts that only support stdio can spawn the same binary:

```bash
export LLMGATEWAY_CLIENT_API_KEY="client-key"
llmgateway mcp --stdio
```

The stdio frontend talks to the already-running local gateway using the same Agent Control and compatibility APIs. This keeps one routing engine and one source of truth.

## Tool surface

| Tool | Permission | Purpose |
|---|---|---|
| `llmgateway_health` | READ | gateway health |
| `llmgateway_capabilities` | READ | client-visible capability/model summary |
| `llmgateway_resolve` | READ | dry-run Router with semantic requirements |
| `llmgateway_diagnostics` | READ | normalized blockers and next action |
| `llmgateway_models` | READ | compatibility model discovery |
| `llmgateway_responses` | EXECUTE | Responses inference |
| `llmgateway_chat` | EXECUTE | Chat Completions inference |
| `llmgateway_messages` | EXECUTE | Anthropic Messages inference |

No OPERATE/ADMIN mutation tools are exposed.

## Capability-aware workflow

1. discover capabilities;
2. dry-run resolve semantic requirements;
3. execute with the same requirements;
4. diagnose only if blocked.

Example:

```json
{
  "capabilities": ["coding", "reasoning"],
  "min_context_window": 32000
}
```

The MCP frontend never chooses providers itself. It forwards semantic constraints into the existing llmgateway Router.

## Generic stdio host config

```json
{
  "command": "/absolute/path/to/llmgateway",
  "args": ["mcp", "--stdio"],
  "env": {
    "LLMGATEWAY_BASE_URL": "http://127.0.0.1:7331",
    "LLMGATEWAY_CLIENT_API_KEY": "<client-key>"
  }
}
```

## Security

- no cookie/token extraction;
- no credential-return tools;
- no account/group deletion;
- no enable/disable mutation;
- no browser restart/re-auth MCP tool;
- client policy remains authoritative;
- semantic requirements can narrow route eligibility but cannot broaden policy.

## Native verification

Relevant gates:

```bash
RUSTFLAGS="-D warnings" cargo check --all-targets
cargo clippy --all-targets
cargo test --all-targets
bash scripts/smoke-agent-control.sh
bash scripts/smoke-native-agent-mcp.sh
```
