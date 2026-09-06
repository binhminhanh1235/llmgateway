#!/usr/bin/env python3
"""
Stress Test & Accuracy Benchmark for Gemini models on LLMGateway.
Evaluates:
  1. Accuracy & Correctness (Math, Logic, JSON Extraction, Python Code Generation, Factual Recall)
  2. Performance & Stability (Sequential burst, Concurrency / Parallel load, Latency P50/P95, Error Rate)
  3. Streaming Stability (TTFB, chunk integrity, [DONE] termination)
  4. Gateway Transport Health (direct-http persistence, zero-Chromium verification)
"""

import argparse
import concurrent.futures
import json
import os
import re
import statistics
import sys
import time
import urllib.error
import urllib.request

DEFAULT_BASE_URL = os.environ.get("LLMGATEWAY_BASE_URL", "http://127.0.0.1:7331")
DEFAULT_API_KEY = os.environ.get("LLMGATEWAY_API_KEY", "LLMGATEWAY_API_KEY")

BENCHMARK_PROMPTS = [
    {
        "id": "math_multi_step",
        "category": "Math",
        "prompt": "Calculate: 37 * 48 - 159. Answer ONLY with the final integer number, no explanation, no words.",
        "eval_type": "exact_int",
        "expected": 1617,
    },
    {
        "id": "time_arithmetic",
        "category": "Math",
        "prompt": "A train departs at 08:15 and arrives at 13:45. How many total minutes did the journey take? Answer with ONLY the integer number.",
        "eval_type": "exact_int",
        "expected": 330,
    },
    {
        "id": "logic_riddle",
        "category": "Logic",
        "prompt": "Sally has 3 brothers. Each brother has 2 sisters. How many sisters does Sally have? Answer with ONLY the integer number.",
        "eval_type": "exact_int",
        "expected": 1,
    },
    {
        "id": "json_schema",
        "category": "JSON",
        "prompt": 'Extract data from: "Alice is 28 years old and lives in Tokyo." Output a JSON object with exactly two keys: "name" (string) and "age" (integer). Output ONLY valid JSON, no markdown formatting.',
        "eval_type": "json_keys",
        "expected": {"name": "Alice", "age": 28},
    },
    {
        "id": "code_is_prime",
        "category": "Coding",
        "prompt": "Write a Python function `def is_prime(n):` that returns True if integer n is prime and False otherwise. Return ONLY the code inside ```python ... ``` without extra explanation.",
        "eval_type": "python_unit_test",
        "test_cases": [(-5, False), (0, False), (1, False), (2, True), (3, True), (4, False), (17, True), (18, False), (97, True), (100, False)],
    },
    {
        "id": "factual_recall",
        "category": "Factual",
        "prompt": "What is the chemical symbol for Gold? Reply with ONLY the 1-2 letter chemical symbol.",
        "eval_type": "exact_string",
        "expected": "Au",
    },
]


def send_chat_completion(base_url, api_key, model, messages, stream=False, timeout=60):
    url = f"{base_url.rstrip('/')}/v1/chat/completions"
    payload = {
        "model": model,
        "messages": messages,
        "stream": stream,
        "temperature": 0.0,
    }
    data = json.dumps(payload).encode("utf-8")
    req = urllib.request.Request(
        url,
        data=data,
        headers={
            "Authorization": f"Bearer {api_key}",
            "Content-Type": "application/json",
        },
        method="POST",
    )

    t0 = time.perf_counter()
    if not stream:
        try:
            with urllib.request.urlopen(req, timeout=timeout) as resp:
                raw = resp.read().decode("utf-8")
                elapsed = time.perf_counter() - t0
                parsed = json.loads(raw)
                content = (
                    parsed.get("choices", [{}])[0].get("message", {}).get("content", "")
                )
                route = resp.headers.get("x-llmgateway-route", "unknown")
                return {
                    "ok": True,
                    "status_code": resp.status,
                    "elapsed_sec": elapsed,
                    "content": content,
                    "route": route,
                    "raw": parsed,
                }
        except urllib.error.HTTPError as e:
            elapsed = time.perf_counter() - t0
            err_body = e.read().decode("utf-8", errors="replace")
            return {
                "ok": False,
                "status_code": e.code,
                "elapsed_sec": elapsed,
                "error": f"HTTP {e.code}: {err_body}",
            }
        except Exception as e:
            elapsed = time.perf_counter() - t0
            return {
                "ok": False,
                "status_code": 0,
                "elapsed_sec": elapsed,
                "error": str(e),
            }
    else:
        # Streaming mode
        ttfb = None
        chunks = []
        full_text = []
        has_done = False
        finish_reason = None
        route = "unknown"
        try:
            with urllib.request.urlopen(req, timeout=timeout) as resp:
                route = resp.headers.get("x-llmgateway-route", "unknown")
                for line in resp:
                    if ttfb is None:
                        ttfb = time.perf_counter() - t0
                    line_str = line.decode("utf-8", errors="replace").strip()
                    if not line_str:
                        continue
                    if line_str == "data: [DONE]":
                        has_done = True
                        break
                    if line_str.startswith("data: "):
                        data_part = line_str[6:]
                        try:
                            frame = json.loads(data_part)
                            chunks.append(frame)
                            choice = frame.get("choices", [{}])[0]
                            delta = choice.get("delta", {}).get("content", "")
                            if delta:
                                full_text.append(delta)
                            if choice.get("finish_reason"):
                                finish_reason = choice.get("finish_reason")
                        except json.JSONDecodeError:
                            pass
                total_time = time.perf_counter() - t0
                return {
                    "ok": True,
                    "status_code": 200,
                    "elapsed_sec": total_time,
                    "ttfb_sec": ttfb or total_time,
                    "chunks_count": len(chunks),
                    "content": "".join(full_text),
                    "finish_reason": finish_reason,
                    "has_done": has_done,
                    "route": route,
                }
        except Exception as e:
            total_time = time.perf_counter() - t0
            return {
                "ok": False,
                "status_code": 0,
                "elapsed_sec": total_time,
                "ttfb_sec": ttfb,
                "error": str(e),
            }


def evaluate_accuracy(task, content):
    if not content:
        return False, "Empty content"
    eval_type = task["eval_type"]
    text = content.strip()

    if eval_type == "exact_int":
        ints = re.findall(r"-?\d+", text)
        if ints:
            val = int(ints[-1])
            expected = task["expected"]
            if val == expected:
                return True, f"Matched {val}"
            return False, f"Expected {expected}, got {val} (full: {text[:60]})"
        return False, f"No integer found in: {text[:60]}"

    elif eval_type == "exact_string":
        expected = task["expected"].strip().lower()
        cleaned = re.sub(r"[^\w\s]", "", text).strip().lower()
        if expected in cleaned.split() or cleaned == expected:
            return True, f"Matched '{task['expected']}'"
        return False, f"Expected '{task['expected']}', got '{text[:60]}'"

    elif eval_type == "json_keys":
        json_match = re.search(r"\{.*\}", text, re.DOTALL)
        if not json_match:
            return False, f"No JSON block found in {text[:60]}"
        try:
            parsed = json.loads(json_match.group(0))
            for k, v in task["expected"].items():
                if k not in parsed:
                    return False, f"Missing key {k}"
                if str(parsed[k]).strip().lower() != str(v).strip().lower():
                    return False, f"Key {k}: expected {v}, got {parsed[k]}"
            return True, "Valid JSON and matched expected fields"
        except Exception as e:
            return False, f"JSON parse error: {e}"

    elif eval_type == "python_unit_test":
        code_match = re.search(r"```(?:python)?\s*(.*?)```", text, re.DOTALL)
        code_str = code_match.group(1) if code_match else text
        if "def is_prime" not in code_str:
            return False, "Function `is_prime` not found in response"
        local_env = {}
        try:
            exec(code_str, {}, local_env)
            fn = local_env.get("is_prime")
            if not callable(fn):
                return False, "is_prime is not callable"
            for inp, exp in task["test_cases"]:
                res = fn(inp)
                if res != exp:
                    return False, f"Failed on input {inp}: expected {exp}, got {res}"
            return True, f"Passed all {len(task['test_cases'])} unit test cases"
        except Exception as e:
            return False, f"Execution failed: {e}"

    return False, "Unknown eval_type"


def get_runtime_status(base_url, api_key, account_id="gemini-web-e68f3020"):
    url = f"{base_url.rstrip('/')}/_llmgateway/browser-accounts/{account_id}/runtime"
    req = urllib.request.Request(
        url, headers={"Authorization": f"Bearer {api_key}"}, method="GET"
    )
    try:
        with urllib.request.urlopen(req, timeout=10) as resp:
            return json.loads(resp.read().decode("utf-8"))
    except Exception as e:
        return {"error": str(e)}


def run_benchmark_for_model(model_name, base_url, api_key, sequential_n=4, concurrent_n=4):
    print(f"\n=======================================================")
    print(f" TESTING MODEL: {model_name}")
    print(f"=======================================================")

    results = {
        "model": model_name,
        "accuracy": {},
        "sequential_stress": {},
        "concurrent_stress": {},
        "streaming": {},
    }

    # 1. ACCURACY BENCHMARK
    print(f"\n--- [1/3] ACCURACY BENCHMARK ({len(BENCHMARK_PROMPTS)} test cases) ---")
    correct_count = 0
    accuracy_details = []
    for task in BENCHMARK_PROMPTS:
        messages = [{"role": "user", "content": task["prompt"]}]
        res = send_chat_completion(base_url, api_key, model_name, messages, stream=False, timeout=60)
        if not res["ok"]:
            passed = False
            detail = f"API Error: {res.get('error')}"
        else:
            passed, detail = evaluate_accuracy(task, res["content"])

        if passed:
            correct_count += 1
            status_icon = "PASS"
        else:
            status_icon = "FAIL"

        print(f"  [{status_icon}] {task['category']} ({task['id']}): {detail} ({res['elapsed_sec']:.2f}s)")
        accuracy_details.append({
            "id": task["id"],
            "category": task["category"],
            "passed": passed,
            "elapsed_sec": res["elapsed_sec"],
            "detail": detail,
            "content": res.get("content", "")[:120],
        })

    accuracy_pct = (correct_count / len(BENCHMARK_PROMPTS)) * 100
    results["accuracy"] = {
        "passed": correct_count,
        "total": len(BENCHMARK_PROMPTS),
        "pct": accuracy_pct,
        "details": accuracy_details,
    }
    print(f"  Accuracy Score: {correct_count}/{len(BENCHMARK_PROMPTS)} ({accuracy_pct:.1f}%)")

    # 2. PERFORMANCE & STABILITY (SEQUENTIAL BURST)
    print(f"\n--- [2/3] PERFORMANCE & STABILITY (Sequential {sequential_n} requests) ---")
    seq_latencies = []
    seq_success = 0
    for i in range(sequential_n):
        prompt = f"Ping request {i+1}. Reply with single word PONG."
        messages = [{"role": "user", "content": prompt}]
        res = send_chat_completion(base_url, api_key, model_name, messages, stream=False, timeout=60)
        if res["ok"] and res["content"]:
            seq_success += 1
            seq_latencies.append(res["elapsed_sec"])
            print(f"  Req {i+1}/{sequential_n}: SUCCESS ({res['elapsed_sec']:.2f}s) - Route: {res.get('route')}")
        else:
            print(f"  Req {i+1}/{sequential_n}: FAILED ({res.get('error')})")

    results["sequential_stress"] = {
        "total": sequential_n,
        "success": seq_success,
        "success_rate_pct": (seq_success / sequential_n) * 100 if sequential_n else 0,
        "latencies": seq_latencies,
        "min_sec": min(seq_latencies) if seq_latencies else 0,
        "max_sec": max(seq_latencies) if seq_latencies else 0,
        "mean_sec": statistics.mean(seq_latencies) if seq_latencies else 0,
        "p50_sec": statistics.median(seq_latencies) if seq_latencies else 0,
    }

    # 3. CONCURRENT LOAD STRESS TEST
    print(f"\n--- [3/3] CONCURRENT LOAD STRESS TEST ({concurrent_n} parallel workers) ---")
    def worker(idx):
        prompt = f"Concurrent worker {idx}: Return the single word 'OK'."
        messages = [{"role": "user", "content": prompt}]
        t0 = time.perf_counter()
        res = send_chat_completion(base_url, api_key, model_name, messages, stream=False, timeout=90)
        total_time = time.perf_counter() - t0
        return idx, res, total_time

    concur_latencies = []
    concur_success = 0
    t_start = time.perf_counter()
    with concurrent.futures.ThreadPoolExecutor(max_workers=concurrent_n) as executor:
        futures = [executor.submit(worker, i + 1) for i in range(concurrent_n)]
        for f in concurrent.futures.as_completed(futures):
            idx, res, total_time = f.result()
            if res["ok"] and res["content"]:
                concur_success += 1
                concur_latencies.append(res["elapsed_sec"])
                print(f"  Worker {idx}: SUCCESS ({res['elapsed_sec']:.2f}s) - Route: {res.get('route')}")
            else:
                print(f"  Worker {idx}: FAILED ({res.get('error')})")

    concur_total_wall_time = time.perf_counter() - t_start
    results["concurrent_stress"] = {
        "concurrency": concurrent_n,
        "total": concurrent_n,
        "success": concur_success,
        "success_rate_pct": (concur_success / concurrent_n) * 100 if concurrent_n else 0,
        "latencies": concur_latencies,
        "wall_time_sec": concur_total_wall_time,
        "min_sec": min(concur_latencies) if concur_latencies else 0,
        "max_sec": max(concur_latencies) if concur_latencies else 0,
        "mean_sec": statistics.mean(concur_latencies) if concur_latencies else 0,
    }

    # 4. STREAMING CHECK
    print(f"\n--- [BONUS] STREAMING SSE VERIFICATION ---")
    stream_prompt = "Count from 1 to 5, one number per line."
    stream_res = send_chat_completion(
        base_url, api_key, model_name,
        [{"role": "user", "content": stream_prompt}],
        stream=True, timeout=60
    )
    if stream_res["ok"]:
        print(f"  Streaming: SUCCESS - TTFB: {stream_res.get('ttfb_sec', 0):.2f}s, Total: {stream_res.get('elapsed_sec', 0):.2f}s, Chunks: {stream_res.get('chunks_count', 0)}, Finish: {stream_res.get('finish_reason')}, [DONE]: {stream_res.get('has_done')}")
    else:
        print(f"  Streaming: FAILED - {stream_res.get('error')}")
    results["streaming"] = stream_res

    return results


def main():
    parser = argparse.ArgumentParser(description="Stress test and benchmark Gemini models on LLMGateway")
    parser.add_argument("--base-url", default=DEFAULT_BASE_URL, help="LLMGateway base URL")
    parser.add_argument("--api-key", default=DEFAULT_API_KEY, help="LLMGateway API key")
    parser.add_argument("--models", nargs="+", default=[
        "gemini-web/gemini-web-flash",
        "gemini-web/gemini-web-flash-lite",
        "gemini-web/gemini-web-pro",
    ], help="Models to test")
    parser.add_argument("--sequential", type=int, default=4, help="Sequential requests per model")
    parser.add_argument("--concurrent", type=int, default=3, help="Concurrent workers per model")
    parser.add_argument("--output-json", default="scripts/benchmark_results.json", help="Path to write JSON results")

    args = parser.parse_args()

    print("==================================================================")
    print(" LLMGATEWAY GEMINI BENCHMARK & STABILITY STRESS TEST")
    print(f" Gateway URL: {args.base_url}")
    print(f" Models to evaluate: {', '.join(args.models)}")
    print(f" Sequential burst: {args.sequential} reqs | Concurrent load: {args.concurrent} workers")
    print("==================================================================")

    # Runtime Pre-check
    pre_runtime = get_runtime_status(args.base_url, args.api_key)
    print("\n--- PRE-CHECK GATEWAY RUNTIME ---")
    print(f" Direct Ready: {pre_runtime.get('direct_ready')}")
    print(f" Effective Transport: {pre_runtime.get('effective_transport')}")
    print(f" Chromium Running: {pre_runtime.get('browser_running')}")
    print(f" Discovered Models: {pre_runtime.get('model_catalog', {}).get('count')}")

    all_results = {}
    for model in args.models:
        all_results[model] = run_benchmark_for_model(
            model,
            args.base_url,
            args.api_key,
            sequential_n=args.sequential,
            concurrent_n=args.concurrent,
        )

    # Runtime Post-check
    post_runtime = get_runtime_status(args.base_url, args.api_key)
    print("\n--- POST-CHECK GATEWAY RUNTIME ---")
    print(f" Direct Ready: {post_runtime.get('direct_ready')}")
    print(f" Effective Transport: {post_runtime.get('effective_transport')}")
    print(f" Chromium Running: {post_runtime.get('browser_running')}")
    print(f" Last Execution: {post_runtime.get('last_execution')}")

    # Summary Table
    print("\n=======================================================================================")
    print(" FINAL BENCHMARK SUMMARY")
    print("=======================================================================================")
    header = f"{'Model':<32} | {'Accuracy':<10} | {'Seq P50':<8} | {'Seq Succ':<8} | {'Concur Succ':<11} | {'Stream TTFB':<11}"
    print(header)
    print("-" * len(header))
    for model, res in all_results.items():
        acc = f"{res['accuracy']['passed']}/{res['accuracy']['total']} ({res['accuracy']['pct']:.0f}%)"
        seq_p50 = f"{res['sequential_stress']['p50_sec']:.2f}s"
        seq_succ = f"{res['sequential_stress']['success_rate_pct']:.0f}%"
        con_succ = f"{res['concurrent_stress']['success_rate_pct']:.0f}%"
        ttfb = f"{res['streaming'].get('ttfb_sec', 0):.2f}s" if res['streaming'].get("ok") else "ERR"
        print(f"{model:<32} | {acc:<10} | {seq_p50:<8} | {seq_succ:<8} | {con_succ:<11} | {ttfb:<11}")
    print("=======================================================================================\n")

    # Save output json
    with open(args.output_json, "w", encoding="utf-8") as f:
        json.dump({
            "pre_runtime": pre_runtime,
            "post_runtime": post_runtime,
            "models": all_results,
        }, f, indent=2)
    print(f"Detailed JSON results written to {args.output_json}")


if __name__ == "__main__":
    main()
