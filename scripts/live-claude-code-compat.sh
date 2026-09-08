#!/usr/bin/env bash
set -euo pipefail

BASE_URL="${LLMGATEWAY_URL:-http://127.0.0.1:7331}"
API_KEY="${LLMGATEWAY_API_KEY:?LLMGATEWAY_API_KEY is required}"
MODELS="${CLAUDE_CODE_MODELS:-deepseek-web/deepseek-web-default gemini-web/gemini-web-flash}"
ANTHROPIC_VERSION_VALUE="${ANTHROPIC_VERSION:-2023-06-01}"
ANTHROPIC_BETA_VALUE="${ANTHROPIC_BETA:-mid-conversation-output-config-2026-07-01,mid-conversation-system-clear-at-2026-08-21,prompt-caching-2024-07-31}"
MIN_TOOL_TURNS="${CLAUDE_CODE_MIN_TOOL_TURNS:-20}"
MAX_MODEL_TURNS="${CLAUDE_CODE_MAX_MODEL_TURNS:-32}"

export BASE_URL API_KEY MODELS ANTHROPIC_VERSION_VALUE ANTHROPIC_BETA_VALUE
export MIN_TOOL_TURNS MAX_MODEL_TURNS

python3 <<'PY'
import http.client
import json
import os
import re
import sys
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
min_tool_turns = int(os.environ["MIN_TOOL_TURNS"])
max_model_turns = int(os.environ["MAX_MODEL_TURNS"])

if min_tool_turns < 20:
    raise SystemExit("CLAUDE_CODE_MIN_TOOL_TURNS must be >= 20 for long-horizon acceptance")
if max_model_turns <= min_tool_turns:
    raise SystemExit("CLAUDE_CODE_MAX_MODEL_TURNS must be greater than CLAUDE_CODE_MIN_TOOL_TURNS")


class ProtocolFailure(AssertionError):
    pass


class AgenticCapabilityFailure(AssertionError):
    pass


def messages(model, payload, session_id, agent_id, parent_agent_id):
    body = json.dumps(payload, separators=(",", ":"))
    conn = http.client.HTTPConnection(host, port, timeout=360)
    conn.request(
        "POST",
        f"{prefix}/v1/messages",
        body=body,
        headers={
            "x-api-key": api_key,
            "Authorization": f"Bearer {api_key}",
            "anthropic-version": anthropic_version,
            "anthropic-beta": anthropic_beta,
            "x-claude-code-session-id": session_id,
            "x-claude-code-agent-id": agent_id,
            "x-claude-code-parent-agent-id": parent_agent_id,
            "Content-Type": "application/json",
            "Content-Length": str(len(body.encode())),
        },
    )
    response = conn.getresponse()
    raw_headers = {key.lower(): value for key, value in response.getheaders()}
    if response.status != 200:
        data = response.read().decode("utf-8", "replace")
        conn.close()
        raise ProtocolFailure(f"{model}: HTTP {response.status}: {data}")

    events = []
    event_name = None
    while True:
        line = response.readline()
        if not line:
            break
        line = line.decode("utf-8", "replace").rstrip("\r\n")
        if not line:
            event_name = None
            continue
        if line.startswith("event:"):
            event_name = line[6:].strip()
            continue
        if not line.startswith("data:"):
            continue
        data = line[5:].strip()
        if not data:
            continue
        try:
            value = json.loads(data)
        except json.JSONDecodeError as exc:
            conn.close()
            raise ProtocolFailure(f"{model}: malformed Anthropic SSE JSON: {data}") from exc
        if event_name and "_event" not in value:
            value["_event"] = event_name
        events.append(value)

    conn.close()
    errors = [event for event in events if event.get("type") == "error"]
    if errors:
        raise ProtocolFailure(f"{model}: Anthropic stream error: {errors}")
    if not any(event.get("type") == "message_start" for event in events):
        raise ProtocolFailure(f"{model}: missing message_start")
    if not any(event.get("type") == "message_stop" for event in events):
        raise ProtocolFailure(f"{model}: missing message_stop")
    request_id = raw_headers.get("request-id")
    legacy_request_id = raw_headers.get("x-llmgateway-request-id")
    if not request_id or request_id != legacy_request_id:
        raise ProtocolFailure(
            f"{model}: request-id/x-llmgateway-request-id missing or inconsistent: {raw_headers}"
        )
    return events, raw_headers


def stop_reason(events):
    for event in events:
        if event.get("type") == "message_delta":
            return (event.get("delta") or {}).get("stop_reason")
    return None


def text_output(events):
    return "".join(
        (event.get("delta") or {}).get("text", "")
        for event in events
        if event.get("type") == "content_block_delta"
        and (event.get("delta") or {}).get("type") == "text_delta"
    )


def tool_calls(events):
    starts = {}
    partials = {}
    for event in events:
        if event.get("type") == "content_block_start":
            block = event.get("content_block") or {}
            if block.get("type") == "tool_use":
                starts[event.get("index")] = block
        elif event.get("type") == "content_block_delta":
            delta = event.get("delta") or {}
            if delta.get("type") == "input_json_delta":
                partials.setdefault(event.get("index"), []).append(delta.get("partial_json", ""))

    calls = []
    for index, block in sorted(starts.items(), key=lambda item: item[0]):
        raw = "".join(partials.get(index, []))
        try:
            tool_input = json.loads(raw or "{}")
        except json.JSONDecodeError as exc:
            raise ProtocolFailure(
                f"tool_use {block.get('id')} emitted invalid input_json_delta: {raw}"
            ) from exc
        calls.append(
            {
                "id": block.get("id"),
                "name": block.get("name"),
                "input": tool_input,
            }
        )
    return calls


def assistant_content(events):
    content = []
    text = text_output(events)
    if text:
        content.append({"type": "text", "text": text})
    for call in tool_calls(events):
        if not call["id"] or not call["name"]:
            raise ProtocolFailure(f"tool_use is missing id or name: {call}")
        content.append(
            {
                "type": "tool_use",
                "id": call["id"],
                "name": call["name"],
                "input": call["input"],
            }
        )
    return content


TOOLS = [
    {
        "name": "inspect_repo",
        "description": "Inspect repository structure and return candidate files.",
        "input_schema": {"type": "object", "properties": {}},
    },
    {
        "name": "read_file",
        "description": "Read a UTF-8 repository file.",
        "input_schema": {
            "type": "object",
            "properties": {"path": {"type": "string"}},
            "required": ["path"],
        },
    },
    {
        "name": "search_code",
        "description": "Search repository text.",
        "input_schema": {
            "type": "object",
            "properties": {"query": {"type": "string"}},
            "required": ["query"],
        },
    },
    {
        "name": "edit_file",
        "description": "Apply a focused replacement to a repository file.",
        "input_schema": {
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "old_text": {"type": "string"},
                "new_text": {"type": "string"},
            },
            "required": ["path", "old_text", "new_text"],
        },
    },
    {
        "name": "run_tests",
        "description": "Run the deterministic reliability test suite.",
        "input_schema": {
            "type": "object",
            "properties": {"target": {"type": "string"}},
        },
    },
]


def execute_tool(call, state):
    name = call["name"]
    tool_input = call["input"]
    state["observed"].append(name)

    if name == "inspect_repo":
        state["inspect_seen"] = True
        result = (
            "Repository files: Cargo.toml, src/api.rs, src/gateway.rs, "
            "src/reliability_fixture.rs, tests/reliability.rs. "
            "Investigate the retry fixture with search/read before editing."
        )
    elif name == "search_code":
        state["search_count"] += 1
        query = str(tool_input.get("query", ""))
        if "timeout" in query.lower():
            result = (
                "src/reliability_fixture.rs: REQUEST_TIMEOUT_SECS = 10; "
                "tests/reliability.rs expects 30 after retry handling is corrected."
            )
        else:
            result = (
                "src/reliability_fixture.rs: RETRY_LIMIT = 0; "
                "tests/reliability.rs: retry_limit_should_be_three."
            )
    elif name == "read_file":
        state["read_count"] += 1
        path = str(tool_input.get("path", ""))
        if path.endswith("reliability_fixture.rs"):
            retry = 3 if state["retry_edit_seen"] else 0
            timeout = 30 if state["post_failure_edit_seen"] else 10
            result = (
                f"pub const RETRY_LIMIT: usize = {retry};\n"
                f"pub const REQUEST_TIMEOUT_SECS: u64 = {timeout};\n"
            )
        elif path.endswith("reliability.rs"):
            result = (
                "#[test] retry_limit_should_be_three -> RETRY_LIMIT == 3\n"
                "#[test] request_timeout_should_be_thirty -> REQUEST_TIMEOUT_SECS == 30"
            )
        else:
            result = f"{path or 'file'}: deterministic acceptance fixture; no unrelated change required."
    elif name == "edit_file":
        state["edit_count"] += 1
        new_text = str(tool_input.get("new_text", ""))
        if not state["failed_test_seen"]:
            state["retry_edit_seen"] = True
        else:
            state["post_failure_edit_seen"] = True
        result = (
            "Focused edit recorded. Run tests after the edit. "
            f"Replacement preview: {new_text[:160]}"
        )
    elif name == "run_tests":
        state["test_runs"] += 1
        if not state["failed_test_seen"]:
            state["failed_test_seen"] = True
            result = (
                "FAILED tests/reliability.rs::request_timeout_should_be_thirty\n"
                "assertion failed: expected REQUEST_TIMEOUT_SECS=30, got 10.\n"
                "Use search/read to diagnose this failed test, then make a subsequent focused edit."
            )
        elif not state["post_failure_edit_seen"]:
            result = (
                "FAILED tests/reliability.rs::request_timeout_should_be_thirty\n"
                "No post-failure fix has been applied yet."
            )
        else:
            state["tests_passed"] = True
            result = "PASS reliability suite: 2 passed; 0 failed."
    else:
        raise AgenticCapabilityFailure(f"unknown tool requested: {name}")

    state["tool_turns"] += 1
    coverage = coverage_ready(state)
    if state["tool_turns"] < min_tool_turns:
        result += (
            f"\nAcceptance progress: {state['tool_turns']}/{min_tool_turns} sequential tool turns. "
            "Continue autonomously with one inspect/read/search/edit/test tool at a time. "
            "Do not provide the final answer before the required long-horizon count is reached."
        )
    elif not coverage:
        result += (
            "\nThe minimum turn count is reached, but required inspect/read/search/edit/"
            "failed-test/recovery coverage is incomplete. Continue using tools until it is complete."
        )
    else:
        result += (
            "\nAll long-horizon acceptance checkpoints are now complete. "
            "Return a concise final completion message without another tool call."
        )
    return result


def coverage_ready(state):
    return (
        state["inspect_seen"]
        and state["read_count"] >= 2
        and state["search_count"] >= 2
        and state["edit_count"] >= 2
        and state["retry_edit_seen"]
        and state["failed_test_seen"]
        and state["post_failure_edit_seen"]
        and state["test_runs"] >= 2
        and state["tests_passed"]
        and state["tool_turns"] >= min_tool_turns
    )


def run_model(model):
    safe = re.sub(r"[^a-zA-Z0-9]+", "-", model).strip("-").lower()
    session_id = f"llmgateway-long-horizon-{safe}"
    agent_id = f"acceptance-agent-{safe}"
    parent_agent_id = "acceptance-root"

    history = [
        {
            "role": "user",
            "content": (
                "Autonomously inspect and repair the deterministic reliability fixture. "
                "Use tool_choice=auto only. Start by inspecting the repository, use multiple "
                "search/read operations, make a focused edit, run tests, investigate the deliberately "
                "failed test result, make a subsequent fix, rerun tests, and continue verification. "
                f"The gateway acceptance requires at least {min_tool_turns} sequential tool/model turns. "
                "Use one tool at a time and do not stop early. When the tool results explicitly say all "
                "acceptance checkpoints are complete, return a concise final completion."
            ),
        }
    ]
    state = {
        "tool_turns": 0,
        "inspect_seen": False,
        "read_count": 0,
        "search_count": 0,
        "edit_count": 0,
        "test_runs": 0,
        "retry_edit_seen": False,
        "failed_test_seen": False,
        "post_failure_edit_seen": False,
        "tests_passed": False,
        "observed": [],
        "pings": 0,
        "routes": [],
    }

    for model_turn in range(1, max_model_turns + 1):
        payload = {
            "model": model,
            "max_tokens": 1536,
            "stream": True,
            "system": [
                {
                    "type": "text",
                    "text": (
                        "You are Claude Code running a long-horizon autonomous coding acceptance. "
                        "Use the supplied tools whenever work remains. Treat failed tests as evidence "
                        "to investigate, not as a reason to stop."
                    ),
                    "cache_control": {"type": "ephemeral"},
                }
            ],
            "messages": history,
            "tools": TOOLS,
            "tool_choice": {
                "type": "auto",
                "disable_parallel_tool_use": True,
            },
            "thinking": {"type": "adaptive"},
            "output_config": {"effort": "high"},
            "metadata": {"user_id": "llmgateway-claude-code-long-horizon"},
        }

        events, headers = messages(
            model,
            payload,
            session_id=session_id,
            agent_id=agent_id,
            parent_agent_id=parent_agent_id,
        )
        state["routes"].append(headers.get("x-llmgateway-route"))
        state["pings"] += sum(1 for event in events if event.get("type") == "ping")
        reason = stop_reason(events)
        calls = tool_calls(events)
        content = assistant_content(events)
        history.append({"role": "assistant", "content": content})

        if calls:
            if reason != "tool_use":
                raise ProtocolFailure(
                    f"{model}: tool blocks were emitted but stop_reason={reason!r}, expected tool_use"
                )
            if len(calls) != 1:
                raise AgenticCapabilityFailure(
                    f"{model}: disable_parallel_tool_use was set but model emitted {len(calls)} tools"
                )
            call = calls[0]
            result = execute_tool(call, state)
            history.append(
                {
                    "role": "user",
                    "content": [
                        {
                            "type": "tool_result",
                            "tool_use_id": call["id"],
                            "content": [{"type": "text", "text": result}],
                        }
                    ],
                }
            )
            continue

        final_text = text_output(events).strip()
        if reason == "max_tokens":
            raise AgenticCapabilityFailure(
                f"{model}: model hit max_tokens before autonomous completion at turn {model_turn}"
            )
        if not coverage_ready(state):
            raise AgenticCapabilityFailure(
                f"{model}: normal {reason or 'end_turn'} before long-horizon checklist completed; "
                f"tool_turns={state['tool_turns']} coverage={state}"
            )
        if not final_text:
            raise AgenticCapabilityFailure(f"{model}: final completion contained no text")

        return {
            "model": model,
            "phase": "VERIFIED_AUTONOMOUS_TOOL_LOOP",
            "session_id": session_id,
            "model_turns": model_turn,
            "tool_turns": state["tool_turns"],
            "read_count": state["read_count"],
            "search_count": state["search_count"],
            "edit_count": state["edit_count"],
            "test_runs": state["test_runs"],
            "failed_test_seen": state["failed_test_seen"],
            "post_failure_edit_seen": state["post_failure_edit_seen"],
            "pings_observed": state["pings"],
            "routes": list(dict.fromkeys(route for route in state["routes"] if route)),
            "final_text": final_text[:240],
            "routing_capability": "claude-code-tool-loop-verified",
        }

    raise AgenticCapabilityFailure(
        f"{model}: exceeded {max_model_turns} model turns without final completion"
    )


failures = []
for model in models:
    try:
        result = run_model(model)
        print(json.dumps(result, ensure_ascii=False))
    except ProtocolFailure as exc:
        failures.append((model, "PROTOCOL_FAIL", str(exc)))
        print(json.dumps({"model": model, "phase": "PROTOCOL_FAIL", "error": str(exc)}, ensure_ascii=False))
    except AgenticCapabilityFailure as exc:
        failures.append((model, "AGENTIC_CAPABILITY_FAIL", str(exc)))
        print(
            json.dumps(
                {
                    "model": model,
                    "phase": "AGENTIC_CAPABILITY_FAIL",
                    "error": str(exc),
                    "note": (
                        "Protocol transport may still be healthy. Do not mark this model with "
                        "claude-code-tool-loop-verified until autonomous acceptance passes."
                    ),
                },
                ensure_ascii=False,
            )
        )

if failures:
    print("CLAUDE_CODE_LONG_HORIZON_ACCEPTANCE=FAIL", file=sys.stderr)
    for model, phase, error in failures:
        print(f"{model}: {phase}: {error}", file=sys.stderr)
    raise SystemExit(1)

print("CLAUDE_CODE_LONG_HORIZON_ACCEPTANCE=PASS")
PY
