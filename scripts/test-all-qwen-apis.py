#!/usr/bin/env python3
import json
import os
import sys
import time
import urllib.request
import urllib.error
import http.client

BASE_URL = "http://127.0.0.1:7331"
API_KEY = "LLMGATEWAY_API_KEY"

results = []

def record(category, test_name, status, details=None, latency_ms=0, issue=None):
    entry = {
        "category": category,
        "test": test_name,
        "status": status,
        "latency_ms": round(latency_ms, 2),
        "details": details or {},
        "issue": issue
    }
    results.append(entry)
    icon = "✅" if status == "PASS" else ("⚠️" if status == "WARN" else "❌")
    print(f"{icon} [{category}] {test_name}: {status} ({round(latency_ms, 1)}ms)")
    if issue:
        print(f"   ↳ ISSUE: {issue}")

def request(method, path, body=None, headers=None, expect_status=200):
    url = f"{BASE_URL}{path}"
    req_headers = {
        "Authorization": f"Bearer {API_KEY}",
        "Content-Type": "application/json"
    }
    if headers:
        req_headers.update(headers)
    data = json.dumps(body).encode("utf-8") if body is not None else None
    start = time.monotonic()
    req = urllib.request.Request(url, data=data, headers=req_headers, method=method)
    try:
        with urllib.request.urlopen(req, timeout=45) as resp:
            elapsed = (time.monotonic() - start) * 1000
            resp_body = resp.read().decode("utf-8")
            parsed = None
            try:
                parsed = json.loads(resp_body)
            except Exception:
                parsed = resp_body
            return resp.status, resp.headers, parsed, elapsed
    except urllib.error.HTTPError as err:
        elapsed = (time.monotonic() - start) * 1000
        err_body = err.read().decode("utf-8")
        parsed = None
        try:
            parsed = json.loads(err_body)
        except Exception:
            parsed = err_body
        return err.code, err.headers, parsed, elapsed
    except Exception as exc:
        elapsed = (time.monotonic() - start) * 1000
        return None, {}, str(exc), elapsed

def test_catalog():
    # 1. GET /v1/models
    status, hdrs, body, lat = request("GET", "/v1/models")
    if status == 200 and isinstance(body, dict) and "data" in body:
        qwen_models = [m for m in body["data"] if "qwen" in m.get("id", "").lower()]
        record("Catalog", "GET /v1/models (list models)", "PASS", 
               {"total_models": len(body["data"]), "qwen_models": [m["id"] for m in qwen_models]}, lat)
    else:
        record("Catalog", "GET /v1/models (list models)", "FAIL", {"status": status, "body": body}, lat, "Failed to list models")

    # 2. GET /_llmgateway/models
    status, hdrs, body, lat = request("GET", "/_llmgateway/models")
    if status == 200 and isinstance(body, dict) and "data" in body:
        qwen_models = [m for m in body["data"] if "qwen" in m.get("id", "").lower()]
        record("Catalog", "GET /_llmgateway/models (admin catalog)", "PASS",
               {"qwen_models": [m["id"] for m in qwen_models]}, lat)
    else:
        record("Catalog", "GET /_llmgateway/models (admin catalog)", "FAIL", {"status": status, "body": body}, lat, "Admin models endpoint failed")

def test_accounts():
    # 1. GET /_llmgateway/accounts
    status, hdrs, body, lat = request("GET", "/_llmgateway/accounts")
    if status == 200 and isinstance(body, dict) and "data" in body:
        qwen_accts = [a for a in body["data"] if "qwen" in a.get("id", "").lower()]
        record("Accounts", "GET /_llmgateway/accounts", "PASS",
               {"qwen_accounts": qwen_accts}, lat)
    else:
        record("Accounts", "GET /_llmgateway/accounts", "FAIL", {"status": status, "body": body}, lat, "Failed to list accounts")

    # 2. GET /_llmgateway/accounts/qwen-web-3bf9bd84/models
    status, hdrs, body, lat = request("GET", "/_llmgateway/accounts/qwen-web-3bf9bd84/models")
    if status == 200 and isinstance(body, dict) and "data" in body:
        models = body["data"]
        record("Accounts", "GET /_llmgateway/accounts/qwen-web-3bf9bd84/models", "PASS",
               {"models": [m.get("id") for m in models]}, lat)
    else:
        record("Accounts", "GET /_llmgateway/accounts/qwen-web-3bf9bd84/models", "FAIL", {"status": status, "body": body}, lat)

    # 3. POST /_llmgateway/accounts/qwen-web-3bf9bd84/models/refresh
    status, hdrs, body, lat = request("POST", "/_llmgateway/accounts/qwen-web-3bf9bd84/models/refresh")
    # Note: Does browser-qwen support model discovery?
    if status == 200:
        record("Accounts", "POST /_llmgateway/accounts/qwen-web-3bf9bd84/models/refresh", "PASS", body, lat)
    elif status == 400:
        record("Accounts", "POST /_llmgateway/accounts/qwen-web-3bf9bd84/models/refresh", "WARN", body, lat,
               "Model discovery not supported for browser-qwen or returned 400")
    else:
        record("Accounts", "POST /_llmgateway/accounts/qwen-web-3bf9bd84/models/refresh", "FAIL", body, lat,
               f"Unexpected status {status}")

    # 4. GET /_llmgateway/accounts/qwen-web-3bf9bd84/usage
    status, hdrs, body, lat = request("GET", "/_llmgateway/accounts/qwen-web-3bf9bd84/usage")
    if status == 200 and isinstance(body, dict):
        record("Accounts", "GET /_llmgateway/accounts/qwen-web-3bf9bd84/usage", "PASS", body, lat)
    else:
        record("Accounts", "GET /_llmgateway/accounts/qwen-web-3bf9bd84/usage", "FAIL", {"status": status, "body": body}, lat)

    # 5. POST /_llmgateway/accounts/qwen-web-3bf9bd84/quota/reset
    status, hdrs, body, lat = request("POST", "/_llmgateway/accounts/qwen-web-3bf9bd84/quota/reset")
    if status == 200:
        record("Accounts", "POST /_llmgateway/accounts/qwen-web-3bf9bd84/quota/reset", "PASS", body, lat)
    else:
        record("Accounts", "POST /_llmgateway/accounts/qwen-web-3bf9bd84/quota/reset", "FAIL", {"status": status, "body": body}, lat)

    # 6. GET /_llmgateway/account-intelligence
    status, hdrs, body, lat = request("GET", "/_llmgateway/account-intelligence")
    if status == 200 and isinstance(body, dict):
        accounts = body.get("accounts", {})
        qwen_intel = {k: v for k, v in accounts.items() if "qwen" in k.lower()}
        record("Accounts", "GET /_llmgateway/account-intelligence", "PASS",
               {"qwen_intelligence": qwen_intel}, lat)
    else:
        record("Accounts", "GET /_llmgateway/account-intelligence", "FAIL", {"status": status, "body": body}, lat)

def test_browser_sessions():
    # 1. GET /_llmgateway/browser-sessions
    status, hdrs, body, lat = request("GET", "/_llmgateway/browser-sessions")
    if status == 200 and isinstance(body, dict):
        sessions = [s for s in body.get("sessions", []) if "qwen" in s.get("id", "").lower()]
        record("BrowserSessions", "GET /_llmgateway/browser-sessions", "PASS",
               {"qwen_sessions": sessions}, lat)
    else:
        record("BrowserSessions", "GET /_llmgateway/browser-sessions", "FAIL", body, lat)

    # 2. GET driver/status
    status, hdrs, body, lat = request("GET", "/_llmgateway/browser-sessions/qwen-web-3bf9bd84/driver/status")
    if status == 200 and isinstance(body, dict):
        record("BrowserSessions", "GET driver/status (qwen-web-3bf9bd84)", "PASS",
               {"running": body.get("running"), "debugger_reachable": body.get("debugger_reachable")}, lat)
    else:
        record("BrowserSessions", "GET driver/status (qwen-web-3bf9bd84)", "FAIL", body, lat)

    # 3. POST driver/verify
    status, hdrs, body, lat = request("POST", "/_llmgateway/browser-sessions/qwen-web-3bf9bd84/driver/verify")
    if status == 200 and isinstance(body, dict):
        auth = body.get("authenticated")
        record("BrowserSessions", "POST driver/verify (qwen-web-3bf9bd84)", "PASS" if auth else "WARN",
               {"authenticated": auth, "ready_match": body.get("ready_match")}, lat,
               None if auth else "Not authenticated")
    else:
        record("BrowserSessions", "POST driver/verify (qwen-web-3bf9bd84)", "FAIL", body, lat)

def test_browser_account_setup():
    # 1. GET /_llmgateway/browser-account-setup/providers
    status, hdrs, body, lat = request("GET", "/_llmgateway/browser-account-setup/providers")
    if status == 200 and isinstance(body, dict):
        providers = {p.get("id"): p for p in body.get("providers", [])}
        has_qwen = "qwen" in providers
        record("AccountSetup", "GET /_llmgateway/browser-account-setup/providers", "PASS" if has_qwen else "FAIL",
               {"qwen_preset": providers.get("qwen")}, lat,
               None if has_qwen else "qwen provider preset missing")
    else:
        record("AccountSetup", "GET /_llmgateway/browser-account-setup/providers", "FAIL", body, lat)

    # 2. PATCH enable/disable account
    status, hdrs, body, lat = request("PATCH", "/_llmgateway/browser-account-setup/qwen-web-3bf9bd84", {"enabled": False})
    if status == 200:
        # Re-enable
        st2, h2, b2, lat2 = request("PATCH", "/_llmgateway/browser-account-setup/qwen-web-3bf9bd84", {"enabled": True})
        record("AccountSetup", "PATCH toggle enabled (qwen-web-3bf9bd84)", "PASS" if st2 == 200 else "FAIL",
               {"disabled": body, "re_enabled": b2}, lat + lat2)
    else:
        record("AccountSetup", "PATCH toggle enabled (qwen-web-3bf9bd84)", "FAIL", body, lat)

def test_routing_explain():
    # Test routing explanation for Qwen models
    models_to_test = [
        "qwen-web/qwen-web-default",
        "qwen/qwen3-coder-plus",
        "llmgateway-coding"
    ]
    for model in models_to_test:
        status, hdrs, body, lat = request("POST", "/_llmgateway/routes/explain", {"model": model})
        if status == 200 and isinstance(body, dict):
            sel = body.get("selected_route")
            cands = len(body.get("candidates", []))
            record("Routing", f"POST /routes/explain ({model})", "PASS",
                   {"selected_route": sel, "candidates_count": cands}, lat)
        else:
            record("Routing", f"POST /routes/explain ({model})", "FAIL", body, lat)

def test_chat_completions():
    # 1. Non-streaming
    body = {
        "model": "qwen-web/qwen-web-default",
        "messages": [{"role": "user", "content": "Respond with the word 'HELLO' and nothing else."}],
        "stream": False
    }
    status, hdrs, res, lat = request("POST", "/v1/chat/completions", body)
    if status == 200 and isinstance(res, dict) and "choices" in res:
        content = res["choices"][0]["message"]["content"]
        route = hdrs.get("x-llmgateway-route")
        record("Chat", "POST /v1/chat/completions (non-stream)", "PASS",
               {"route": route, "content": content}, lat)
    else:
        record("Chat", "POST /v1/chat/completions (non-stream)", "FAIL", res, lat, f"HTTP {status}")

    # 2. System message + multi-turn conversation
    body_multi = {
        "model": "qwen-web/qwen-web-default",
        "messages": [
            {"role": "system", "content": "You are an assistant that only speaks in rhymes."},
            {"role": "user", "content": "How is the weather?"}
        ],
        "stream": False
    }
    status, hdrs, res, lat = request("POST", "/v1/chat/completions", body_multi)
    if status == 200 and isinstance(res, dict) and "choices" in res:
        content = res["choices"][0]["message"]["content"]
        record("Chat", "POST /v1/chat/completions (system + multi-turn)", "PASS",
               {"content": content[:100]}, lat)
    else:
        record("Chat", "POST /v1/chat/completions (system + multi-turn)", "FAIL", res, lat, f"HTTP {status}")

    # 3. Tool Calling bridge
    body_tools = {
        "model": "qwen-web/qwen-web-default",
        "messages": [{"role": "user", "content": "Calculate the square root of 144 using calculator tool"}],
        "tools": [
            {
                "type": "function",
                "function": {
                    "name": "calculator",
                    "description": "Perform mathematical calculation",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "expression": {"type": "string"}
                        },
                        "required": ["expression"]
                    }
                }
            }
        ],
        "stream": False
    }
    status, hdrs, res, lat = request("POST", "/v1/chat/completions", body_tools)
    if status == 200 and isinstance(res, dict) and "choices" in res:
        choice = res["choices"][0]
        finish = choice.get("finish_reason")
        tool_calls = choice.get("message", {}).get("tool_calls")
        record("Chat", "POST /v1/chat/completions (tool bridge)", "PASS" if finish == "tool_calls" else "WARN",
               {"finish_reason": finish, "tool_calls": tool_calls, "content": choice.get("message", {}).get("content")}, lat,
               None if finish == "tool_calls" else f"Qwen did not invoke tool: finish_reason={finish}")
    else:
        record("Chat", "POST /v1/chat/completions (tool bridge)", "FAIL", res, lat, f"HTTP {status}")

def test_chat_streaming():
    # Test SSE Streaming over HTTPConnection
    conn = http.client.HTTPConnection("127.0.0.1", 7331, timeout=60)
    body = json.dumps({
        "model": "qwen-web/qwen-web-default",
        "messages": [{"role": "user", "content": "Count from 1 to 3. Just the numbers."}],
        "stream": True
    })
    start = time.monotonic()
    conn.request("POST", "/v1/chat/completions", body=body, headers={
        "Authorization": f"Bearer {API_KEY}",
        "Content-Type": "application/json"
    })
    resp = conn.getresponse()
    elapsed = (time.monotonic() - start) * 1000
    if resp.status != 200:
        record("Streaming", "POST /v1/chat/completions (stream)", "FAIL", {"status": resp.status}, elapsed, f"HTTP {resp.status}")
        return

    route = resp.getheader("x-llmgateway-route")
    chunks = []
    has_done = False
    full_content = ""
    for line in resp:
        line_str = line.decode("utf-8").strip()
        if line_str == "data: [DONE]":
            has_done = True
            break
        if line_str.startswith("data: "):
            try:
                data = json.loads(line_str[6:])
                delta = data.get("choices", [{}])[0].get("delta", {}).get("content", "")
                if delta:
                    full_content += delta
                chunks.append(data)
            except Exception:
                pass
    elapsed = (time.monotonic() - start) * 1000
    record("Streaming", "POST /v1/chat/completions (stream)", "PASS" if has_done else "WARN",
           {"route": route, "chunks_count": len(chunks), "full_content": full_content.strip(), "has_done": has_done},
           elapsed, None if has_done else "Missing [DONE]")

def test_responses_api():
    # OpenAI Responses API (/v1/responses)
    body = {
        "model": "qwen-web/qwen-web-default",
        "input": "Respond with 'RESPONSES_API_OK'"
    }
    status, hdrs, res, lat = request("POST", "/v1/responses", body)
    if status == 200 and isinstance(res, dict):
        output = res.get("output") or res.get("choices", [{}])[0].get("message", {}).get("content")
        record("ResponsesAPI", "POST /v1/responses", "PASS", {"output": str(output)[:80]}, lat)
    else:
        record("ResponsesAPI", "POST /v1/responses", "FAIL", res, lat, f"HTTP {status}")

def test_anthropic_messages():
    # Anthropic /v1/messages API
    body = {
        "model": "qwen-web/qwen-web-default",
        "max_tokens": 100,
        "messages": [
            {"role": "user", "content": "Say: ANTHROPIC_OK"}
        ]
    }
    status, hdrs, res, lat = request("POST", "/v1/messages", body)
    if status == 200 and isinstance(res, dict):
        text = ""
        for c in res.get("content", []):
            if c.get("type") == "text":
                text += c.get("text", "")
        record("AnthropicAPI", "POST /v1/messages (Claude format)", "PASS",
               {"content": text.strip(), "role": res.get("role")}, lat)
    else:
        record("AnthropicAPI", "POST /v1/messages (Claude format)", "FAIL", res, lat, f"HTTP {status}")

def test_threads():
    # 1. Create thread
    status, hdrs, thread, lat = request("POST", "/v1/threads", {"metadata": {"test": "qwen"}})
    if status not in (200, 201) or not isinstance(thread, dict) or "id" not in thread:
        record("Threads", "POST /v1/threads (create)", "FAIL", thread, lat, "Thread creation failed")
        return
    thread_id = thread["id"]
    record("Threads", "POST /v1/threads (create)", "PASS", {"thread_id": thread_id}, lat)

    # 2. Post message to thread with Qwen model
    msg_body = {
        "model": "qwen-web/qwen-web-default",
        "message": {"role": "user", "content": "My secret code is BLUE_ORCHID_99. What is my secret code?"}
    }
    status, hdrs, msg_res, lat = request("POST", f"/v1/threads/{thread_id}/messages", msg_body)
    if status == 200 and isinstance(msg_res, dict):
        reply = msg_res.get("reply") or msg_res.get("message", {}).get("content") or msg_res.get("choices", [{}])[0].get("message", {}).get("content")
        record("Threads", f"POST /v1/threads/{thread_id}/messages (turn 1)", "PASS",
               {"reply": str(reply)[:100]}, lat)
    else:
        record("Threads", f"POST /v1/threads/{thread_id}/messages (turn 1)", "FAIL", msg_res, lat, f"HTTP {status}")

    # 3. Post follow-up message to verify thread memory
    followup_body = {
        "model": "qwen-web/qwen-web-default",
        "message": {"role": "user", "content": "Repeat only the secret code I told you."}
    }
    status, hdrs, f_res, lat = request("POST", f"/v1/threads/{thread_id}/messages", followup_body)
    if status == 200 and isinstance(f_res, dict):
        reply = f_res.get("reply") or f_res.get("message", {}).get("content") or f_res.get("choices", [{}])[0].get("message", {}).get("content")
        has_secret = "BLUE_ORCHID" in str(reply)
        record("Threads", f"POST /v1/threads/{thread_id}/messages (turn 2 - memory recall)",
               "PASS" if has_secret else "WARN",
               {"reply": str(reply)[:100], "recalled_secret": has_secret}, lat,
               None if has_secret else "Thread memory did not recall the secret code")
    else:
        record("Threads", f"POST /v1/threads/{thread_id}/messages (turn 2)", "FAIL", f_res, lat, f"HTTP {status}")

    # 4. Context inspection
    status, hdrs, ctx, lat = request("GET", f"/v1/threads/{thread_id}/context")
    if status == 200 and isinstance(ctx, dict):
        msg_count = len(ctx.get("messages", []))
        record("Threads", f"GET /v1/threads/{thread_id}/context", "PASS",
               {"messages_count": msg_count}, lat)
    else:
        record("Threads", f"GET /v1/threads/{thread_id}/context", "FAIL", ctx, lat)

    # 5. Delete thread
    status, hdrs, del_res, lat = request("DELETE", f"/v1/threads/{thread_id}")
    record("Threads", f"DELETE /v1/threads/{thread_id}", "PASS" if status in (200, 204) else "FAIL", del_res, lat)

def test_executions():
    status, hdrs, execs, lat = request("GET", "/_llmgateway/executions")
    if status == 200 and isinstance(execs, dict) and "data" in execs:
        qwen_execs = [e for e in execs["data"] if "qwen" in (e.get("selected_route") or e.get("requested_model") or "").lower()]
        record("Executions", "GET /_llmgateway/executions", "PASS",
               {"qwen_executions_count": len(qwen_execs)}, lat)
        if qwen_execs:
            req_id = qwen_execs[0]["request_id"]
            st2, h2, detail, lat2 = request("GET", f"/_llmgateway/executions/{req_id}")
            record("Executions", f"GET /_llmgateway/executions/{req_id}", "PASS" if st2 == 200 else "FAIL",
                   {"attempts": len(detail.get("attempts", [])) if isinstance(detail, dict) else 0}, lat2)
    else:
        record("Executions", "GET /_llmgateway/executions", "FAIL", execs, lat)

def test_api_provider_fallback():
    # Test DashScope Qwen API provider with empty key:
    # Model: qwen/qwen3-coder-plus (configured under qwen-primary with QWEN_API_KEY empty)
    body = {
        "model": "qwen/qwen3-coder-plus",
        "messages": [{"role": "user", "content": "Hi"}],
        "stream": False
    }
    status, hdrs, res, lat = request("POST", "/v1/chat/completions", body)
    record("EdgeCases", "POST /v1/chat/completions (qwen/qwen3-coder-plus with empty QWEN_API_KEY)",
           "PASS" if status in (401, 502, 503) else ("WARN" if status == 200 else "FAIL"),
           {"status": status, "response": res}, lat,
           f"Empty QWEN_API_KEY produced HTTP {status}")

if __name__ == "__main__":
    print("=== STARTING COMPREHENSIVE QWEN API TEST SUITE ===\n")
    test_catalog()
    test_accounts()
    test_browser_sessions()
    test_browser_account_setup()
    test_routing_explain()
    test_chat_completions()
    test_chat_streaming()
    test_responses_api()
    test_anthropic_messages()
    test_threads()
    test_executions()
    test_api_provider_fallback()

    print("\n=== SUMMARY OF ISSUES & IMPROVEMENT OPPORTUNITIES ===")
    issues = [r for r in results if r["status"] in ("FAIL", "WARN") or r["issue"]]
    for r in issues:
        print(f"- [{r['category']}] {r['test']}: status={r['status']}, issue={r['issue']}")

    with open("scripts/qwen_api_test_results.json", "w") as f:
        json.dump(results, f, indent=2)
    print("\nDetailed results saved to scripts/qwen_api_test_results.json")
