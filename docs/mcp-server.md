# llmgateway MCP server

Status: P2 implementation on `feat/agent-native-runtime`; tracking issue #92.

## Goal

Expose llmgateway to MCP hosts without teaching each host provider-specific routing or giving it unrestricted admin controls.

The MCP bridge is dependency-free Python:

```text
skills/llmgateway/mcp/llmgateway_mcp.py
```

## Run

Gateway must already be running.

```bash
export LLMGATEWAY_BASE_URL="http://127.0.0.1:7331"
export LLMGATEWAY_CLIENT_API_KEY="client-key"
python3 skills/llmgateway/mcp/llmgateway_mcp.py
```

Use the global admin key only when the client truly needs legacy/admin access. The default MCP tool set does not require admin mutations.

## Transport and protocol

The bridge speaks newline-delimited JSON-RPC over stdio.

It supports:

- modern discovery: `server/discover`;
- legacy initialization: `initialize`;
- `ping`;
- `tools/list`;
- `tools/call`.

Modern discovery advertises protocol `2026-07-28` plus legacy compatibility versions.

## Tool surface

| Tool | Permission | Purpose |
|---|---|---|
| `llmgateway_capabilities` | READ | client-visible capability/model summary |
| `llmgateway_resolve` | READ | dry-run Router with semantic requirements |
| `llmgateway_diagnostics` | READ | normalized blockers and next action |
| `llmgateway_models` | READ | compatibility model discovery |
| `llmgateway_responses` | EXECUTE | Responses inference |
| `llmgateway_chat` | EXECUTE | Chat Completions inference |
| `llmgateway_messages` | EXECUTE | Anthropic Messages inference |

No OPERATE/ADMIN tools are exposed by default.

## Capability-aware workflow

1. `llmgateway_capabilities`
2. `llmgateway_resolve` with semantic requirements
3. execute using the **same** task/capabilities/context requirement
4. if blocked, `llmgateway_diagnostics`

Example requirements:

```json
{
  "capabilities": ["coding", "reasoning"],
  "min_context_window": 32000
}
```

The MCP bridge forwards these constraints to llmgateway. It does not resolve providers itself.

## Generic host configuration

A host that accepts stdio MCP server configuration can launch:

```json
{
  "command": "python3",
  "args": ["skills/llmgateway/mcp/llmgateway_mcp.py"],
  "env": {
    "LLMGATEWAY_BASE_URL": "http://127.0.0.1:7331",
    "LLMGATEWAY_CLIENT_API_KEY": "<client-key>"
  }
}
```

Use an absolute path to the script when the host runs from another working directory.

## Security

- no cookie/token extraction;
- no credential-return tools;
- no account/group deletion;
- no enable/disable mutation;
- no browser restart/re-auth tool;
- client policy remains authoritative;
- requirements cannot broaden allowed models/routes/transports.

## Tests

```bash
python3 -m unittest skills/llmgateway/tests/test_llmgateway_mcp.py
bash scripts/smoke-agent-control.sh
```
