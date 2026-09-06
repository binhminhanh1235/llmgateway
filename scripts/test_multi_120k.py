#!/usr/bin/env python3
import urllib.request
import json
import time
import sys

BASE_URL = "http://127.0.0.1:7331/v1/chat/completions"
HEADERS = {
    "Content-Type": "application/json",
    "Authorization": "Bearer LLMGATEWAY_API_KEY"
}

def make_call(test_name, payload, is_stream=True):
    print(f"\n==========================================")
    print(f"RUNNING: {test_name}")
    print(f"Stream: {is_stream}")
    total_prompt_len = sum(len(m["content"]) for m in payload["messages"])
    print(f"Total prompt characters: {total_prompt_len}")
    
    req = urllib.request.Request(
        BASE_URL,
        data=json.dumps(payload).encode("utf-8"),
        headers=HEADERS,
        method="POST"
    )
    
    start_time = time.time()
    try:
        with urllib.request.urlopen(req, timeout=90) as resp:
            elapsed = time.time() - start_time
            if is_stream:
                full_text = ""
                last_finish = None
                chunk_count = 0
                for line in resp:
                    line_str = line.decode("utf-8").strip()
                    if line_str.startswith("data: ") and line_str != "data: [DONE]":
                        try:
                            d = json.loads(line_str[6:])
                            c = d["choices"][0]
                            delta = c.get("delta", {})
                            if delta.get("content"):
                                full_text += delta["content"]
                                chunk_count += 1
                            if c.get("finish_reason"):
                                last_finish = c["finish_reason"]
                        except Exception:
                            pass
                print(f"Status: {resp.status} in {elapsed:.2f}s | Chunks: {chunk_count}")
                print(f"Response: {full_text[:120]}..." if len(full_text) > 120 else f"Response: {full_text}")
                print(f"Finish Reason: {last_finish}")
                assert resp.status == 200, f"Expected 200, got {resp.status}"
                assert last_finish == "stop", f"Expected finish_reason 'stop', got '{last_finish}'"
                assert len(full_text.strip()) > 0, "Response text should not be empty"
                print(">>> PASSED")
                return True, elapsed, full_text
            else:
                data = json.loads(resp.read().decode("utf-8"))
                choice = data["choices"][0]
                text = choice["message"]["content"]
                finish = choice["finish_reason"]
                print(f"Status: {resp.status} in {elapsed:.2f}s")
                print(f"Response: {text}")
                print(f"Finish Reason: {finish}")
                assert resp.status == 200
                assert finish == "stop"
                assert len(text.strip()) > 0
                print(">>> PASSED")
                return True, elapsed, text
    except Exception as e:
        elapsed = time.time() - start_time
        print(f"FAILED after {elapsed:.2f}s with error: {e}", file=sys.stderr)
        return False, elapsed, str(e)

def run_all():
    results = []

    # Test 1: Under budget (70,000 chars) -> No truncation expected
    p1 = {
        "model": "gemini-web/gemini-web-flash",
        "messages": [
            {"role": "system", "content": "Bạn là trợ lý ảo ngắn gọn."},
            {"role": "user", "content": "X" * 70000},
            {"role": "user", "content": "Hãy trả lời đúng chữ 'OK1'"}
        ],
        "stream": True
    }
    results.append(("Test 1: Under budget 70k chars (Stream)", make_call("Test 1: Under budget 70k chars (Stream)", p1, is_stream=True)))
    time.sleep(2)

    # Test 2: Over budget (140,000 chars) -> Truncation expected (120k budget)
    p2 = {
        "model": "gemini-web/gemini-web-flash",
        "messages": [
            {"role": "system", "content": "Bạn là trợ lý ảo ngắn gọn."},
            {"role": "user", "content": "Y" * 140000},
            {"role": "user", "content": "Hãy trả lời đúng chữ 'OK2'"}
        ],
        "stream": True
    }
    results.append(("Test 2: Over budget 140k chars (Stream)", make_call("Test 2: Over budget 140k chars (Stream)", p2, is_stream=True)))
    time.sleep(2)

    # Test 3: Massive payload (220,000 chars) -> Safe truncation & no 1155
    p3 = {
        "model": "gemini-web/gemini-web-flash",
        "messages": [
            {"role": "system", "content": "Bạn là trợ lý ảo ngắn gọn."},
            {"role": "user", "content": "Z" * 220000},
            {"role": "user", "content": "Hãy trả lời đúng chữ 'OK3'"}
        ],
        "stream": True
    }
    results.append(("Test 3: Massive payload 220k chars (Stream)", make_call("Test 3: Massive payload 220k chars (Stream)", p3, is_stream=True)))
    time.sleep(2)

    # Test 4: Multi-turn conversation (Non-streaming)
    p4 = {
        "model": "gemini-web/gemini-web-flash",
        "messages": [
            {"role": "user", "content": "Thủ đô của Việt Nam là gì?"},
            {"role": "assistant", "content": "Thủ đô của Việt Nam là Hà Nội."},
            {"role": "user", "content": "Thành phố đó có hồ nào nổi tiếng nhất? Trả lời 1 câu ngắn."}
        ],
        "stream": False
    }
    results.append(("Test 4: Multi-turn conversation (Non-stream)", make_call("Test 4: Multi-turn conversation (Non-stream)", p4, is_stream=False)))
    time.sleep(2)

    # Test 5 & 6: Rapid sequential calls
    p5 = {
        "model": "gemini-web/gemini-web-flash",
        "messages": [
            {"role": "user", "content": "3 + 5 bằng mấy? Trả lời 1 con số."}
        ],
        "stream": True
    }
    results.append(("Test 5: Rapid Call A (Stream)", make_call("Test 5: Rapid Call A (Stream)", p5, is_stream=True)))

    p6 = {
        "model": "gemini-web/gemini-web-flash",
        "messages": [
            {"role": "user", "content": "7 * 8 bằng mấy? Trả lời 1 con số."}
        ],
        "stream": True
    }
    results.append(("Test 6: Rapid Call B (Stream)", make_call("Test 6: Rapid Call B (Stream)", p6, is_stream=True)))

    print("\n==========================================")
    print("ALL TESTS SUMMARY:")
    all_ok = True
    for name, (ok, elapsed, resp) in results:
        status = "PASSED" if ok else "FAILED"
        if not ok:
            all_ok = False
        print(f"[{status}] {name} ({elapsed:.2f}s)")
    
    if all_ok:
        print("\nALL 6 CALLS COMPLETED SUCCESSFULLY!")
    else:
        print("\nSOME CALLS FAILED!")
        sys.exit(1)

if __name__ == "__main__":
    run_all()
