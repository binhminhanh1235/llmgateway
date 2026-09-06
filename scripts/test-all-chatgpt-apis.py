#!/usr/bin/env python3
import json
import os
import sys
import time
import urllib.request
import urllib.error
import http.client

BASE_URL = os.environ.get("LLMGATEWAY_BASE_URL", "http://127.0.0.1:7331")
API_KEY = os.environ.get("LLMGATEWAY_API_KEY", "LLMGATEWAY_API_KEY")
ACCOUNT_ID = os.environ.get("CHATGPT_ACCOUNT_ID", "chatgpt-web-41d15799")
SESSION_ID = os.environ.get("CHATGPT_SESSION_ID", "chatgpt-web-41d15799")
MODEL_ID = os.environ.get("CHATGPT_MODEL_ID", "chatgpt-web/chatgpt-web-default")

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
        with urllib.request.urlopen(req, timeout=60) as resp:
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
        chatgpt_models = [m for m in body["data"] if "chatgpt" in m.get("id", "").lower()]
        record("Catalog", "GET /v1/models (list models)", "PASS",
               {"total_models": len(body["data"]), "chatgpt_models": [m["id"] for m in chatgpt_models]}, lat)
    else:
        record("Catalog", "GET /v1/models (list models)", "FAIL", {"status": status, "body": body}, lat, "Failed to list models")

    # 2. GET /_llmgateway/models
    status, hdrs, body, lat = request("GET", "/_llmgateway/models")
    if status == 200 and isinstance(body, dict) and "data" in body:
        chatgpt_models = [m for m in body["data"] if "chatgpt" in m.get("id", "").lower()]
        record("Catalog", "GET /_llmgateway/models (admin catalog)", "PASS",
               {"chatgpt_models": [m["id"] for m in chatgpt_models]}, lat)
    else:
        record("Catalog", "GET /_llmgateway/models (admin catalog)", "FAIL", {"status": status, "body": body}, lat, "Admin models endpoint failed")

def test_accounts():
    # 1. GET /_llmgateway/accounts
    status, hdrs, body, lat = request("GET", "/_llmgateway/accounts")
    if status == 200 and isinstance(body, dict) and "data" in body:
        chatgpt_accts = [a for a in body["data"] if "chatgpt" in a.get("id", "").lower()]
        record("Accounts", "GET /_llmgateway/accounts", "PASS",
               {"chatgpt_accounts": chatgpt_accts}, lat)
    else:
        record("Accounts", "GET /_llmgateway/accounts", "FAIL", {"status": status, "body": body}, lat, "Failed to list accounts")

    # 2. GET /_llmgateway/accounts/{ACCOUNT_ID}/models
    status, hdrs, body, lat = request("GET", f"/_llmgateway/accounts/{ACCOUNT_ID}/models")
    if status == 200 and isinstance(body, dict) and "data" in body:
        models = body["data"]
        record("Accounts", f"GET /_llmgateway/accounts/{ACCOUNT_ID}/models", "PASS",
               {"models": [m.get("id") for m in models]}, lat)
    else:
        record("Accounts", f"GET /_llmgateway/accounts/{ACCOUNT_ID}/models", "FAIL", {"status": status, "body": body}, lat)

    # 3. POST /_llmgateway/accounts/{ACCOUNT_ID}/models/refresh
    status, hdrs, body, lat = request("POST", f"/_llmgateway/accounts/{ACCOUNT_ID}/models/refresh")
    if status == 200:
        record("Accounts", f"POST /_llmgateway/accounts/{ACCOUNT_ID}/models/refresh", "PASS", body, lat)
    elif status == 400:
        record("Accounts", f"POST /_llmgateway/accounts/{ACCOUNT_ID}/models/refresh", "WARN", body, lat,
               "Model discovery not supported for browser-chatgpt (expected for browser accounts)")
    else:
        record("Accounts", f"POST /_llmgateway/accounts/{ACCOUNT_ID}/models/refresh", "FAIL", body, lat,
               f"Unexpected status {status}")

    # 4. GET /_llmgateway/accounts/{ACCOUNT_ID}/usage
    status, hdrs, body, lat = request("GET", f"/_llmgateway/accounts/{ACCOUNT_ID}/usage")
    if status == 200 and isinstance(body, dict):
        record("Accounts", f"GET /_llmgateway/accounts/{ACCOUNT_ID}/usage", "PASS", body, lat)
    else:
        record("Accounts", f"GET /_llmgateway/accounts/{ACCOUNT_ID}/usage", "FAIL", {"status": status, "body": body}, lat)

    # 5. POST /_llmgateway/accounts/{ACCOUNT_ID}/quota/reset
    status, hdrs, body, lat = request("POST", f"/_llmgateway/accounts/{ACCOUNT_ID}/quota/reset")
    if status == 200:
        record("Accounts", f"POST /_llmgateway/accounts/{ACCOUNT_ID}/quota/reset", "PASS", body, lat)
    else:
        record("Accounts", f"POST /_llmgateway/accounts/{ACCOUNT_ID}/quota/reset", "FAIL", {"status": status, "body": body}, lat)

    # 6. GET /_llmgateway/accounts/{ACCOUNT_ID}/transport
    status, hdrs, body, lat = request("GET", f"/_llmgateway/accounts/{ACCOUNT_ID}/transport")
    if status == 200 and isinstance(body, dict):
        record("Accounts", f"GET /_llmgateway/accounts/{ACCOUNT_ID}/transport", "PASS", body, lat)
    else:
        record("Accounts", f"GET /_llmgateway/accounts/{ACCOUNT_ID}/transport", "FAIL", {"status": status, "body": body}, lat)

    # 7. PATCH /_llmgateway/accounts/{ACCOUNT_ID}/transport
    status, hdrs, body, lat = request("PATCH", f"/_llmgateway/accounts/{ACCOUNT_ID}/transport", {"transport_policy": "browser-only"})
    if status == 200 and isinstance(body, dict):
        record("Accounts", f"PATCH /_llmgateway/accounts/{ACCOUNT_ID}/transport", "PASS", body, lat)
    else:
        record("Accounts", f"PATCH /_llmgateway/accounts/{ACCOUNT_ID}/transport", "FAIL", {"status": status, "body": body}, lat)

    # 8. GET /_llmgateway/account-intelligence
    status, hdrs, body, lat = request("GET", "/_llmgateway/account-intelligence")
    if status == 200 and isinstance(body, dict):
        accounts = body.get("accounts", {})
        chatgpt_intel = {k: v for k, v in accounts.items() if "chatgpt" in k.lower()}
        record("Accounts", "GET /_llmgateway/account-intelligence", "PASS",
               {"chatgpt_intelligence": chatgpt_intel}, lat)
    else:
        record("Accounts", "GET /_llmgateway/account-intelligence", "FAIL", {"status": status, "body": body}, lat)

def test_browser_sessions():
    # 1. GET /_llmgateway/browser-sessions
    status, hdrs, body, lat = request("GET", "/_llmgateway/browser-sessions")
    if status == 200 and isinstance(body, dict):
        sessions = [s for s in body.get("sessions", []) if "chatgpt" in s.get("id", "").lower()]
        record("BrowserSessions", "GET /_llmgateway/browser-sessions", "PASS",
               {"chatgpt_sessions": sessions}, lat)
    else:
        record("BrowserSessions", "GET /_llmgateway/browser-sessions", "FAIL", body, lat)

    # 2. GET /_llmgateway/browser-sessions/{SESSION_ID}
    status, hdrs, body, lat = request("GET", f"/_llmgateway/browser-sessions/{SESSION_ID}")
    if status == 200 and isinstance(body, dict):
        record("BrowserSessions", f"GET /_llmgateway/browser-sessions/{SESSION_ID}", "PASS", body, lat)
    else:
        record("BrowserSessions", f"GET /_llmgateway/browser-sessions/{SESSION_ID}", "FAIL", body, lat)

    # 3. GET /_llmgateway/browser-sessions/{SESSION_ID}/driver/status
    status, hdrs, body, lat = request("GET", f"/_llmgateway/browser-sessions/{SESSION_ID}/driver/status")
    if status == 200 and isinstance(body, dict):
        record("BrowserSessions", f"GET driver/status ({SESSION_ID})", "PASS",
               {"running": body.get("running"), "debugger_reachable": body.get("debugger_reachable")}, lat)
    else:
        record("BrowserSessions", f"GET driver/status ({SESSION_ID})", "FAIL", body, lat)

    # 4. POST /_llmgateway/browser-sessions/{SESSION_ID}/driver/verify
    status, hdrs, body, lat = request("POST", f"/_llmgateway/browser-sessions/{SESSION_ID}/driver/verify")
    if status == 200 and isinstance(body, dict):
        auth = body.get("authenticated")
        record("BrowserSessions", f"POST driver/verify ({SESSION_ID})", "PASS" if auth else "WARN",
               {"authenticated": auth, "ready_match": body.get("ready_match")}, lat,
               None if auth else "Not authenticated or waiting for user login")
    else:
        record("BrowserSessions", f"POST driver/verify ({SESSION_ID})", "FAIL", body, lat)

    # 5. GET /_llmgateway/browser-accounts/{ACCOUNT_ID}/runtime
    status, hdrs, body, lat = request("GET", f"/_llmgateway/browser-accounts/{ACCOUNT_ID}/runtime")
    if status == 200 and isinstance(body, dict):
        record("BrowserSessions", f"GET /_llmgateway/browser-accounts/{ACCOUNT_ID}/runtime", "PASS", body, lat)
    else:
        record("BrowserSessions", f"GET /_llmgateway/browser-accounts/{ACCOUNT_ID}/runtime", "FAIL", body, lat)

def test_browser_account_setup():
    # 1. GET /_llmgateway/browser-account-setup/providers
    status, hdrs, body, lat = request("GET", "/_llmgateway/browser-account-setup/providers")
    if status == 200 and isinstance(body, dict):
        providers = {p.get("id"): p for p in body.get("providers", [])}
        has_chatgpt = "chatgpt" in providers
        record("AccountSetup", "GET /_llmgateway/browser-account-setup/providers", "PASS" if has_chatgpt else "FAIL",
               {"chatgpt_preset": providers.get("chatgpt")}, lat,
               None if has_chatgpt else "chatgpt provider preset missing")
    else:
        record("AccountSetup", "GET /_llmgateway/browser-account-setup/providers", "FAIL", body, lat)

    # 2. PATCH enable/disable account
    status, hdrs, body, lat = request("PATCH", f"/_llmgateway/browser-account-setup/{ACCOUNT_ID}", {"enabled": False})
    if status == 200:
        # Re-enable
        st2, h2, b2, lat2 = request("PATCH", f"/_llmgateway/browser-account-setup/{ACCOUNT_ID}", {"enabled": True})
        record("AccountSetup", f"PATCH toggle enabled ({ACCOUNT_ID})", "PASS" if st2 == 200 else "FAIL",
               {"disabled": body, "re_enabled": b2}, lat + lat2)
    else:
        record("AccountSetup", f"PATCH toggle enabled ({ACCOUNT_ID})", "FAIL", body, lat)

def test_routing_explain():
    # Test routing explanation for ChatGPT models
    models_to_test = [
        MODEL_ID,
        "llmgateway-auto",
        "llmgateway-best"
    ]
    for model in models_to_test:
        status, hdrs, body, lat = request("POST", "/_llmgateway/routes/explain", {"model": model})
        if status == 200 and isinstance(body, dict):
            sel = body.get("selected_route")
            cands = len(body.get("candidates", []))
            record("Routing", f"POST /_llmgateway/routes/explain ({model})", "PASS",
                   {"selected_route": sel, "candidates_count": cands}, lat)
        else:
            record("Routing", f"POST /_llmgateway/routes/explain ({model})", "FAIL", body, lat)

def test_chat_completions():
    # 1. Non-streaming
    body = {
        "model": MODEL_ID,
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
        "model": MODEL_ID,
        "messages": [
            {"role": "system", "content": "You are a concise assistant."},
            {"role": "user", "content": "What is 2+2? Answer in one word."}
        ],
        "stream": False
    }
    status, hdrs, res, lat = request("POST", "/v1/chat/completions", body_multi)
    if status == 200 and isinstance(res, dict) and "choices" in res:
        content = res["choices"][0]["message"]["content"]
        record("Chat", "POST /v1/chat/completions (system + multi-turn)", "PASS",
               {"content": str(content)[:100]}, lat)
    else:
        record("Chat", "POST /v1/chat/completions (system + multi-turn)", "FAIL", res, lat, f"HTTP {status}")

    # 3. Tool Calling bridge
    body_tools = {
        "model": MODEL_ID,
        "messages": [{"role": "user", "content": "Calculate 12 * 12 using the calculator tool"}],
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
               None if finish == "tool_calls" else f"ChatGPT did not invoke tool: finish_reason={finish}")
    else:
        record("Chat", "POST /v1/chat/completions (tool bridge)", "FAIL", res, lat, f"HTTP {status}")

def test_chat_streaming():
    # Test SSE Streaming over HTTPConnection
    parsed_url = urllib.parse.urlparse(BASE_URL)
    host = parsed_url.hostname or "127.0.0.1"
    port = parsed_url.port or 7331
    conn = http.client.HTTPConnection(host, port, timeout=90)
    body = json.dumps({
        "model": MODEL_ID,
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
        "model": MODEL_ID,
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
        "model": MODEL_ID,
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
    status, hdrs, thread, lat = request("POST", "/v1/threads", {"title": "ChatGPT Test Thread", "model": MODEL_ID})
    if status not in (200, 201) or not isinstance(thread, dict) or "id" not in thread:
        record("Threads", "POST /v1/threads (create)", "FAIL", thread, lat, "Thread creation failed")
        return
    thread_id = thread["id"]
    record("Threads", "POST /v1/threads (create)", "PASS", {"thread_id": thread_id}, lat)

    # 2. Inspect thread
    status, hdrs, thr_info, lat = request("GET", f"/v1/threads/{thread_id}")
    record("Threads", f"GET /v1/threads/{thread_id}", "PASS" if status == 200 else "FAIL", thr_info, lat)

    # 3. Post message to thread (Turn 1)
    msg_body = {
        "model": MODEL_ID,
        "content": "My secret code is CRIMSON_LOTUS_42. What is my secret code?",
        "stream": False
    }
    status, hdrs, msg_res, lat = request("POST", f"/v1/threads/{thread_id}/messages", msg_body)
    if status == 200 and isinstance(msg_res, dict):
        reply = msg_res.get("reply") or msg_res.get("content") or msg_res.get("choices", [{}])[0].get("message", {}).get("content")
        record("Threads", f"POST /v1/threads/{thread_id}/messages (turn 1 non-stream)", "PASS",
               {"reply": str(reply)[:100]}, lat)
    else:
        record("Threads", f"POST /v1/threads/{thread_id}/messages (turn 1 non-stream)", "FAIL", msg_res, lat, f"HTTP {status}")

    # 4. Check Thread Affinity diagnostics
    status, hdrs, aff_info, lat = request("GET", f"/_llmgateway/threads/{thread_id}/browser-affinity/{ACCOUNT_ID}")
    record("Threads", f"GET thread browser-affinity ({ACCOUNT_ID})", "PASS" if status == 200 else "WARN",
           aff_info, lat, None if status == 200 else f"HTTP {status}")

    # 5. Post follow-up message with stream=True
    msg2_body = {
        "model": MODEL_ID,
        "content": "Repeat only the secret code I gave you earlier.",
        "stream": True
    }
    status, hdrs, msg2_res, lat = request("POST", f"/v1/threads/{thread_id}/messages", msg2_body)
    record("Threads", f"POST /v1/threads/{thread_id}/messages (turn 2 stream)",
           "PASS" if status == 200 else "FAIL", msg2_res, lat, None if status == 200 else f"HTTP {status}")

    # 6. Context inspection
    status, hdrs, ctx, lat = request("GET", f"/v1/threads/{thread_id}/context")
    if status == 200 and isinstance(ctx, dict):
        msg_count = len(ctx.get("messages", []))
        record("Threads", f"GET /v1/threads/{thread_id}/context", "PASS",
               {"messages_count": msg_count}, lat)
    else:
        record("Threads", f"GET /v1/threads/{thread_id}/context", "FAIL", ctx, lat)

    # 7. Context compaction
    status, hdrs, comp_res, lat = request("POST", f"/v1/threads/{thread_id}/compact", {"model": MODEL_ID})
    record("Threads", f"POST /v1/threads/{thread_id}/compact", "PASS" if status in (200, 204) else "WARN",
           comp_res, lat)

    # 8. Delete thread
    status, hdrs, del_res, lat = request("DELETE", f"/v1/threads/{thread_id}")
    record("Threads", f"DELETE /v1/threads/{thread_id}", "PASS" if status in (200, 204) else "FAIL", del_res, lat)

def test_executions():
    status, hdrs, execs, lat = request("GET", "/_llmgateway/executions")
    if status == 200 and isinstance(execs, dict) and "data" in execs:
        chatgpt_execs = [e for e in execs["data"] if "chatgpt" in (e.get("selected_route") or e.get("requested_model") or "").lower()]
        record("Executions", "GET /_llmgateway/executions", "PASS",
               {"chatgpt_executions_count": len(chatgpt_execs)}, lat)
        if chatgpt_execs:
            req_id = chatgpt_execs[0]["request_id"]
            st2, h2, detail, lat2 = request("GET", f"/_llmgateway/executions/{req_id}")
            record("Executions", f"GET /_llmgateway/executions/{req_id}", "PASS" if st2 == 200 else "FAIL",
                   {"attempts": len(detail.get("attempts", [])) if isinstance(detail, dict) else 0}, lat2)
    else:
        record("Executions", "GET /_llmgateway/executions", "FAIL", execs, lat)

if __name__ == "__main__":
    import urllib.parse
    print(f"=== STARTING COMPREHENSIVE CHATGPT API TEST SUITE ===")
    print(f"Base URL:   {BASE_URL}")
    print(f"Account ID: {ACCOUNT_ID}")
    print(f"Model ID:   {MODEL_ID}\n")

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

    print("\n=== SUMMARY OF ISSUES & IMPROVEMENT OPPORTUNITIES ===")
    issues = [r for r in results if r["status"] in ("FAIL", "WARN") or r["issue"]]
    for r in issues:
        print(f"- [{r['category']}] {r['test']}: status={r['status']}, issue={r['issue']}")

    with open("scripts/chatgpt_api_test_results.json", "w") as f:
        json.dump(results, f, indent=2)
    print("\nDetailed results saved to scripts/chatgpt_api_test_results.json")
