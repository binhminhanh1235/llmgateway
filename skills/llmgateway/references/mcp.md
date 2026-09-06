# MCP integration

llmgateway ships MCP inside the same Rust executable. No Python or separate MCP package is required.

## Preferred transport: HTTP

Start llmgateway normally:

```bash
llmgateway
```

The native MCP endpoint is:

```text
POST http://127.0.0.1:7331/mcp
```

Use the same scoped client credential you use for inference.

Modern MCP `2026-07-28` requests are stateless. Send:

- `MCP-Protocol-Version: 2026-07-28`
- `Mcp-Method: <json-rpc method>`
- `Mcp-Name: <tool name>` for `tools/call`
- the normal llmgateway Authorization header.

## Local stdio fallback

For MCP hosts that require stdio, configure the same executable:

```bash
export LLMGATEWAY_BASE_URL="http://127.0.0.1:7331"
export LLMGATEWAY_CLIENT_API_KEY="client-key"
llmgateway mcp --stdio
```

The stdio mode is a native Rust frontend. It does not require Python, Node.js, pip, npm, or a second installed application.

## Tools

- `llmgateway_health`
- `llmgateway_capabilities`
- `llmgateway_resolve`
- `llmgateway_diagnostics`
- `llmgateway_models`
- `llmgateway_responses`
- `llmgateway_chat`
- `llmgateway_messages`

There are no delete, enable/disable, credential, quota-reset, or browser-runtime mutation MCP tools.

## Routing rule

For tasks with hard requirements:

1. call `llmgateway_capabilities`;
2. call `llmgateway_resolve`;
3. execute with the same `task`, `capabilities`, and `min_context_window`;
4. call `llmgateway_diagnostics` if resolution is blocked.

Do not resolve with one requirement set and execute without it.

## Host configuration

HTTP-capable MCP hosts should point directly at:

```text
http://127.0.0.1:7331/mcp
```

For stdio-only hosts:

```json
{
  "command": "llmgateway",
  "args": ["mcp", "--stdio"],
  "env": {
    "LLMGATEWAY_BASE_URL": "http://127.0.0.1:7331",
    "LLMGATEWAY_CLIENT_API_KEY": "<client-key>"
  }
}
```

Prefer HTTP when the host supports modern MCP because it uses the already-running llmgateway process.
