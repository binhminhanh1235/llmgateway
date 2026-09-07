#!/usr/bin/env bash
set -euo pipefail

BASE_URL="${LLMGATEWAY_URL:-http://127.0.0.1:7331}"
API_KEY="${LLMGATEWAY_API_KEY:?LLMGATEWAY_API_KEY is required}"
MODELS="${CLAUDE_CODE_MODELS:-deepseek-web/deepseek-web-default gemini-web/gemini-web-flash}"
ANTHROPIC_VERSION_VALUE="${ANTHROPIC_VERSION:-2023-06-01}"
ANTHROPIC_BETA_VALUE="${ANTHROPIC_BETA:-mid-conversation-output-config-2026-07-01,mid-conversation-system-clear-at-2026-08-21,prompt-caching-2024-07-31}"

export BASE_URL API_KEY MODELS ANTHROPIC_VERSION_VALUE ANTHROPIC_BETA_VALUE

python3 <<'PY'
import http.client
import json
import os
from urllib.parse import urlparse

base = urlparse(os.environ["BASE_URL"])
if base.scheme != "http":
    raise SystemExit("live Claude Code compatibility runner currently requires an http:// LLMGateway URL")
host = base.hostname or "127.0.0.1"
port = base.port or 80
prefix = base.path.rstrip("/")
api_key = os.environ["API_KEY"]
models = os.environ["MODELS"].split()
anthropic_version = os.environ["ANTHROPIC_VERSION_VALUE"]
anthropic_beta = os.environ["ANTHROPIC_BETA_VALUE"]


def messages(model, payload):
    body = json.dumps(payload, separators=(",", ":"))
    conn = http.client.HTTPConnection(host, port, timeout=180)
    conn.request(
        "POST",
        f"{prefix}/v1/messages",
        body=body,
        headers={
            "x-api-key": api_key,
            "Authorization": f"Bearer {api_key}",
            "anthropic-version": anthropic_version,
            "anthropic-beta": anthropic_beta,
            "Content-Type": "application/json",
            "Content-Length": str(len(body.encode())),
        },
    )
    response = conn.getresponse()
    raw_headers = dict(response.getheaders())
    if response.status != 200:
        data = response.read().decode("utf-8", "replace")
        conn.close()
        raise AssertionError(f"{model}: HTTP {response.status}: {data}")

    events = []
    event_name = None
    while True:
        line = response.readline()
        if not line:
            break
        line = line.decode("utf-8", "replace").rstrip("\r\n")
        if line.startswith("event:"):
            event_name = line[6:].strip()
            continue
        if not line.startswith("data:"):
            continue
        data = line[5:].strip()
        if not data:
            continue
        if data == "[DONE]":
            break
        value = json.loads(data)
        if event_name and "type" not in value:
            value["_event"] = event_name
        events.append(value)

    conn.close()
    errors = [event for event in events if event.get("type") == "error"]
    if errors:
        raise AssertionError(f"{model}: Anthropic stream error: {errors}")
    if not any(event.get("type") == "message_start" for event in events):
        raise AssertionError(f"{model}: missing message_start: {events}")
    if not any(event.get("type") == "message_stop" for event in events):
        raise AssertionError(f"{model}: missing message_stop: {events}")
    if not raw_headers.get("x-llmgateway-request-id"):
        raise AssertionError(f"{model}: missing x-llmgateway-request-id")
    return events, raw_headers


def tool_start(events):
    for event in events:
        if event.get("type") != "content_block_start":
            continue
        block = event.get("content_block") or {}
        if block.get("type") == "tool_use":
            return block
    return None


def text_output(events):
    return "".join(
        (event.get("delta") or {}).get("text", "")
        for event in events
        if event.get("type") == "content_block_delta"
        and (event.get("delta") or {}).get("type") == "text_delta"
    )


tool = {
    "name": "read_file",
    "description": "Read a UTF-8 repository file",
    "input_schema": {
        "type": "object",
        "properties": {"path": {"type": "string"}},
        "required": ["path"],
    },
    "cache_control": {"type": "ephemeral"},
}

for model in models:
    first_payload = {
        "model": model,
        "max_tokens": 1024,
        "stream": True,
        "system": [{
            "type": "text",
            "text": (
                "You are Claude Code. When a tool is forced, return the requested tool call "
                "instead of explaining how to call it."
            ),
            "cache_control": {"type": "ephemeral"},
        }],
        "messages": [{
            "role": "user",
            "content": [{"type": "text", "text": "Read Cargo.toml using the read_file tool."}],
        }],
        "tools": [tool],
        "tool_choice": {
            "type": "tool",
            "name": "read_file",
            "disable_parallel_tool_use": True,
        },
        "thinking": {"type": "adaptive"},
        "output_config": {"effort": "high"},
        "metadata": {"user_id": "llmgateway-claude-code-live-acceptance"},
        "unknown_optional_beta_field": {"accepted": True},
    }

    first_events, first_headers = messages(model, first_payload)
    started_tool = tool_start(first_events)
    if not started_tool:
        raise AssertionError(
            f"{model}: forced read_file tool call was not emitted: {first_events}"
        )
    if started_tool.get("name") != "read_file":
        raise AssertionError(f"{model}: unexpected tool name: {started_tool}")

    tool_id = started_tool.get("id")
    if not tool_id:
        raise AssertionError(f"{model}: tool_use block has no id: {started_tool}")

    second_payload = {
        "model": model,
        "max_tokens": 1024,
        "stream": True,
        "system": "You are Claude Code. Continue from tool results faithfully.",
        "messages": [
            {"role": "user", "content": "Read Cargo.toml using the read_file tool."},
            {
                "role": "assistant",
                "content": [{
                    "type": "tool_use",
                    "id": tool_id,
                    "name": "read_file",
                    "input": {"path": "Cargo.toml"},
                }],
            },
            {
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": tool_id,
                    "content": [{
                        "type": "text",
                        "text": "[package]\nname = \"llmgateway\"\nversion = \"0.32.0\"",
                    }],
                }],
            },
            {
                "role": "system",
                "clear_at": "next_user_message",
                "content": "For the next user message, answer in one concise sentence.",
            },
            {
                "role": "system",
                "content": [],
                "output_config": {"effort": "low"},
            },
            {"role": "user", "content": "What package name did the tool return?"},
        ],
        "tools": [tool],
        "tool_choice": {"type": "none"},
        "cache_control": {"type": "ephemeral"},
        "future_optional_beta": {"enabled": True},
    }

    second_events, second_headers = messages(model, second_payload)
    final_text = text_output(second_events).strip()
    if not final_text:
        raise AssertionError(f"{model}: follow-up produced no text: {second_events}")

    print(
        json.dumps(
            {
                "model": model,
                "phase": "VERIFIED",
                "first_route": first_headers.get("x-llmgateway-route"),
                "second_route": second_headers.get("x-llmgateway-route"),
                "tool_id": tool_id,
                "follow_up_text": final_text[:160],
            },
            ensure_ascii=False,
        )
    )

print("CLAUDE_CODE_COMPAT_LIVE_ACCEPTANCE=PASS")
PY
