# MCP integration

The bundled MCP server is:

`skills/llmgateway/mcp/llmgateway_mcp.py`

It uses newline-delimited JSON-RPC over stdio and is dependency-free.

## Start

```bash
export LLMGATEWAY_BASE_URL="http://127.0.0.1:7331"
export LLMGATEWAY_CLIENT_API_KEY="client-key"
python3 skills/llmgateway/mcp/llmgateway_mcp.py
```

Use a scoped client key whenever possible. The MCP server intentionally exposes only READ + EXECUTE tools.

## Tools

- `llmgateway_capabilities`
- `llmgateway_resolve`
- `llmgateway_diagnostics`
- `llmgateway_models`
- `llmgateway_responses`
- `llmgateway_chat`
- `llmgateway_messages`

There are no delete, enable/disable, credential, quota-reset, or browser-runtime mutation tools.

## Protocol compatibility

The server supports modern MCP discovery with `server/discover` and also the legacy `initialize` handshake used by older hosts.

## Routing rule

For tasks with hard requirements:

1. call `llmgateway_capabilities`;
2. call `llmgateway_resolve`;
3. execute with the same `task`, `capabilities`, and `min_context_window` arguments;
4. call `llmgateway_diagnostics` when resolution is blocked.

Do not resolve with one requirement set and then execute without it.
