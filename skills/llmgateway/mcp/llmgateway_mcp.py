#!/usr/bin/env python3
"""Dependency-free MCP stdio bridge for llmgateway.

Supports modern MCP 2026-07-28 discovery and legacy initialize-era clients.
The exposed tool set is intentionally READ + EXECUTE only.
"""

from __future__ import annotations

import importlib.util
import json
import os
import pathlib
import sys
from typing import Any


ROOT = pathlib.Path(__file__).resolve().parents[1]
HELPER = ROOT / "scripts" / "llmgateway_agent.py"
SPEC = importlib.util.spec_from_file_location("llmgateway_agent", HELPER)
MODULE = importlib.util.module_from_spec(SPEC)
assert SPEC.loader is not None
SPEC.loader.exec_module(MODULE)

MODERN_VERSION = "2026-07-28"
LEGACY_VERSION = "2025-11-25"
SERVER_NAME = "llmgateway"
SERVER_VERSION = "0.32-agent-native"

TOOLS: list[dict[str, Any]] = [
    {
        "name": "llmgateway_capabilities",
        "description": "List client-visible logical/physical models and capability metadata.",
        "inputSchema": {"type": "object", "properties": {}, "additionalProperties": False},
    },
    {
        "name": "llmgateway_resolve",
        "description": "Resolve semantic capability requirements through the existing llmgateway Router without executing inference.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "model": {"type": "string"},
                "task": {"type": "string"},
                "capabilities": {"type": "array", "items": {"type": "string"}},
                "min_context_window": {"type": "integer", "minimum": 1},
                "body": {"type": "object"},
            },
            "additionalProperties": False,
        },
    },
    {
        "name": "llmgateway_diagnostics",
        "description": "Return a normalized route diagnostic snapshot and recommended next action.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "model": {"type": "string"},
                "task": {"type": "string"},
                "capabilities": {"type": "array", "items": {"type": "string"}},
                "min_context_window": {"type": "integer", "minimum": 1},
                "body": {"type": "object"},
            },
            "additionalProperties": False,
        },
    },
    {
        "name": "llmgateway_models",
        "description": "List models visible to the current llmgateway execution credential.",
        "inputSchema": {"type": "object", "properties": {}, "additionalProperties": False},
    },
    {
        "name": "llmgateway_responses",
        "description": "Execute a non-streaming OpenAI Responses request through llmgateway.",
        "inputSchema": {
            "type": "object",
            "required": ["prompt"],
            "properties": {
                "model": {"type": "string", "default": "llmgateway-auto"},
                "prompt": {"type": "string"},
                "task": {"type": "string"},
                "capabilities": {"type": "array", "items": {"type": "string"}},
                "min_context_window": {"type": "integer", "minimum": 1},
            },
            "additionalProperties": False,
        },
    },
    {
        "name": "llmgateway_chat",
        "description": "Execute a non-streaming OpenAI Chat Completions request through llmgateway.",
        "inputSchema": {
            "type": "object",
            "required": ["prompt"],
            "properties": {
                "model": {"type": "string", "default": "llmgateway-auto"},
                "prompt": {"type": "string"},
                "task": {"type": "string"},
                "capabilities": {"type": "array", "items": {"type": "string"}},
                "min_context_window": {"type": "integer", "minimum": 1},
            },
            "additionalProperties": False,
        },
    },
    {
        "name": "llmgateway_messages",
        "description": "Execute a non-streaming Anthropic Messages request through llmgateway.",
        "inputSchema": {
            "type": "object",
            "required": ["prompt"],
            "properties": {
                "model": {"type": "string", "default": "llmgateway-coding"},
                "prompt": {"type": "string"},
                "task": {"type": "string"},
                "capabilities": {"type": "array", "items": {"type": "string"}},
                "min_context_window": {"type": "integer", "minimum": 1},
                "max_tokens": {"type": "integer", "minimum": 1, "default": 1024},
            },
            "additionalProperties": False,
        },
    },
]


def gateway_client() -> Any:
    execution_key = os.environ.get("LLMGATEWAY_CLIENT_API_KEY") or os.environ.get(
        "LLMGATEWAY_API_KEY"
    )
    return MODULE.GatewayClient(
        os.environ.get("LLMGATEWAY_BASE_URL", MODULE.DEFAULT_BASE_URL),
        execution_key=execution_key,
        admin_key=os.environ.get("LLMGATEWAY_API_KEY"),
        timeout=float(os.environ.get("LLMGATEWAY_MCP_TIMEOUT", "60")),
    )


def route_payload(arguments: dict[str, Any]) -> dict[str, Any]:
    payload: dict[str, Any] = {
        "requirements": {
            "capabilities": arguments.get("capabilities", []),
            "min_context_window": arguments.get("min_context_window"),
        }
    }
    for key in ("model", "task", "body"):
        if key in arguments and arguments[key] is not None:
            payload[key] = arguments[key]
    return payload


def call_tool(name: str, arguments: dict[str, Any]) -> Any:
    client = gateway_client()
    if name == "llmgateway_capabilities":
        return client._request("GET", "/_llmgateway/agent/capabilities", auth="execution")
    if name == "llmgateway_resolve":
        return client._request(
            "POST",
            "/_llmgateway/agent/resolve",
            body=route_payload(arguments),
            auth="execution",
        )
    if name == "llmgateway_diagnostics":
        return client._request(
            "POST",
            "/_llmgateway/agent/diagnostics",
            body=route_payload(arguments),
            auth="execution",
        )
    if name == "llmgateway_models":
        return client.models()
    if name == "llmgateway_responses":
        return client.responses(
            arguments.get("model") or "llmgateway-auto",
            required_string(arguments, "prompt"),
            task=arguments.get("task"),
            capabilities=arguments.get("capabilities") or [],
            min_context_window=arguments.get("min_context_window"),
        )
    if name == "llmgateway_chat":
        return client.chat(
            arguments.get("model") or "llmgateway-auto",
            required_string(arguments, "prompt"),
            task=arguments.get("task"),
            capabilities=arguments.get("capabilities") or [],
            min_context_window=arguments.get("min_context_window"),
        )
    if name == "llmgateway_messages":
        max_tokens = arguments.get("max_tokens", 1024)
        if not isinstance(max_tokens, int) or max_tokens < 1:
            raise ValueError("max_tokens must be a positive integer")
        return client.messages(
            arguments.get("model") or "llmgateway-coding",
            required_string(arguments, "prompt"),
            max_tokens,
            task=arguments.get("task"),
            capabilities=arguments.get("capabilities") or [],
            min_context_window=arguments.get("min_context_window"),
        )
    raise KeyError(name)


def required_string(arguments: dict[str, Any], key: str) -> str:
    value = arguments.get(key)
    if not isinstance(value, str) or not value.strip():
        raise ValueError(f"{key} must be a non-empty string")
    return value


def result_for_tool(value: Any) -> dict[str, Any]:
    text_value = json.dumps(value, ensure_ascii=False, separators=(",", ":"))
    return {
        "resultType": "complete",
        "content": [{"type": "text", "text": text_value}],
        "structuredContent": value,
        "isError": False,
    }


def error_result(message: str) -> dict[str, Any]:
    return {
        "resultType": "complete",
        "content": [{"type": "text", "text": message}],
        "isError": True,
    }


def dispatch(message: dict[str, Any]) -> dict[str, Any] | None:
    if message.get("jsonrpc") != "2.0":
        return rpc_error(message.get("id"), -32600, "Invalid Request")

    method = message.get("method")
    request_id = message.get("id")
    params = message.get("params") or {}
    if not isinstance(params, dict):
        return rpc_error(request_id, -32602, "Invalid params")

    if method == "notifications/initialized" or method == "notifications/cancelled":
        return None

    if method == "server/discover":
        return rpc_result(
            request_id,
            {
                "resultType": "complete",
                "supportedVersions": [MODERN_VERSION, LEGACY_VERSION, "2025-06-18"],
                "capabilities": {"tools": {}},
                "serverInfo": {"name": SERVER_NAME, "version": SERVER_VERSION},
                "instructions": (
                    "Use capability/resolve tools before hard-coding provider models. "
                    "This MCP server exposes READ + EXECUTE only; it does not delete, "
                    "disable, or mutate llmgateway configuration."
                ),
            },
        )

    if method == "initialize":
        requested = params.get("protocolVersion")
        protocol = requested if requested in {LEGACY_VERSION, "2025-06-18"} else LEGACY_VERSION
        return rpc_result(
            request_id,
            {
                "protocolVersion": protocol,
                "capabilities": {"tools": {"listChanged": False}},
                "serverInfo": {"name": SERVER_NAME, "version": SERVER_VERSION},
                "instructions": (
                    "Use llmgateway_resolve for capability-aware routing. "
                    "Mutation/admin tools are intentionally not exposed."
                ),
            },
        )

    if method == "ping":
        return rpc_result(request_id, {})

    if method == "tools/list":
        return rpc_result(
            request_id,
            {
                "tools": TOOLS,
                "_meta": {
                    "io.modelcontextprotocol/serverInfo": {
                        "name": SERVER_NAME,
                        "version": SERVER_VERSION,
                    }
                },
            },
        )

    if method == "tools/call":
        name = params.get("name")
        arguments = params.get("arguments") or {}
        if not isinstance(name, str) or not isinstance(arguments, dict):
            return rpc_error(request_id, -32602, "tools/call requires name and arguments")
        if name not in {tool["name"] for tool in TOOLS}:
            return rpc_error(request_id, -32601, f"Unknown tool: {name}")
        try:
            return rpc_result(request_id, result_for_tool(call_tool(name, arguments)))
        except (MODULE.GatewayError, ValueError) as exc:
            return rpc_result(request_id, error_result(str(exc)))
        except Exception as exc:
            return rpc_result(request_id, error_result(f"llmgateway MCP tool failed: {exc}"))

    return rpc_error(request_id, -32601, f"Method not found: {method}")


def rpc_result(request_id: Any, result: Any) -> dict[str, Any]:
    return {"jsonrpc": "2.0", "id": request_id, "result": result}


def rpc_error(request_id: Any, code: int, message: str) -> dict[str, Any]:
    return {
        "jsonrpc": "2.0",
        "id": request_id,
        "error": {"code": code, "message": message},
    }


def serve() -> int:
    for raw in sys.stdin:
        raw = raw.strip()
        if not raw:
            continue
        try:
            message = json.loads(raw)
            if not isinstance(message, dict):
                response = rpc_error(None, -32600, "Invalid Request")
            else:
                response = dispatch(message)
        except json.JSONDecodeError:
            response = rpc_error(None, -32700, "Parse error")
        if response is not None:
            sys.stdout.write(json.dumps(response, ensure_ascii=False, separators=(",", ":")) + "\n")
            sys.stdout.flush()
    return 0


if __name__ == "__main__":
    raise SystemExit(serve())
