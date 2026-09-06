#!/usr/bin/env python3
"""
Concurrent Long-Thread Multi-Turn Stress Test for Gemini on LLMGateway.
Tests:
  - 2 parallel chat threads running multi-turn conversations
  - Prompts dispatched simultaneously across both threads at each turn
  - Long context ingestion (~1000+ words per prompt)
  - Memory & Context isolation check (secret key recall with zero cross-talk)
  - Complex multi-step reasoning and python code synthesis
  - Native Gemini conversation affinity tracking and zero-Chromium direct-http verification
"""

import argparse
import concurrent.futures
import datetime
import json
import os
import re
import sys
import time
import urllib.error
import urllib.request

DEFAULT_BASE_URL = os.environ.get("LLMGATEWAY_BASE_URL", "http://127.0.0.1:7331")
DEFAULT_API_KEY = os.environ.get("LLMGATEWAY_API_KEY", "LLMGATEWAY_API_KEY")

THREAD_A_CONTEXT = """
[SYSTEM DOMAIN: QUANTUM COMPUTING & CRYPTOGRAPHY]
SECRET IDENTIFIER: SECRET_KEY_QUANTUM_77889

Theoretical foundations of quantum computation rely on the superposition principle and quantum entanglement. While classical computing operates on deterministic binary digits (bits), quantum information processors utilize quantum bits (qubits) existing in arbitrary state vectors |ψ⟩ = α|0⟩ + β|1⟩ within a two-dimensional complex Hilbert space. The primary operational advantage emerges from unitary transformations acting simultaneously across the state space.

In 1994, Peter Shor formulated a polynomial-time quantum algorithm for integer factorization and discrete logarithms. For an integer N of n bits, Shor's algorithm determines prime factors in O((log N)^3) operations, which critically undermines the security posture of classical public-key cryptography including RSA (Rivest-Shamir-Adleman), Diffie-Hellman key exchange, and Elliptic Curve Cryptography (ECDSA/ECDH). Consequently, the global cryptanalytic landscape necessitates an immediate transition toward Post-Quantum Cryptography (PQC).

The primary mathematical paradigms underpinning post-quantum cryptographic primitives are:
1. Lattice-based cryptography: Grounded in hard computational lattice problems such as Learning With Errors (LWE), Ring-LWE, Module-LWE, and the Shortest Vector Problem (SVP). Prominent examples standardized by NIST include ML-KEM (Kyber) and ML-DSA (Dilithium). Lattice cryptography offers exceptional versatility, supporting both public-key encryption and fully homomorphic encryption (FHE), although public key and ciphertext sizes are notably larger than classical counterparts.
2. Code-based cryptography: Pioneered by Robert McEliece in 1978 using Goppa codes. Code-based systems feature exceptionally high decapsulation speeds and strong security guarantees, but suffer from extremely large public key sizes (often hundreds of kilobytes).
3. Hash-based cryptography: Relies strictly on the collision-resistance and pre-image resistance of cryptographic hash functions (such as SHA-256 or SHAKE-256). Implementations like SPHINCS+ and LMS/XMSS provide stateless or stateful signature schemes with minimal algebraic attack surfaces, though signature byte-sizes are relatively expansive.
4. Isogeny-based cryptography: Historically explored through supersingular elliptic curve isogenies (CSIDH, SQISign), providing compact keys at the expense of substantial computational latency and ongoing cryptanalytic maturation.
5. Multivariate quadratic cryptography: Based on the NP-hardness of solving systems of multivariate polynomials over finite fields, offering rapid verification but substantial public key footprints.

Please confirm receipt of this domain background and state the 5 primary post-quantum mathematical paradigms listed above in a clean numbered list.
"""

THREAD_B_CONTEXT = """
[SYSTEM DOMAIN: MACROECONOMICS & FINANCIAL CRISIS DYNAMICS]
SECRET IDENTIFIER: SECRET_KEY_MACRO_33441

The architecture of modern financial intermediation depends on liquidity transformation and fractional reserve banking. Financial institutions operate by borrowing short (liquid demand deposits and wholesale repo funding) and lending long (illiquid mortgages, corporate credit, and infrastructure debt). When aggregate confidence deteriorates, this structural maturity mismatch exposes the financial network to severe endogenous liquidity spirals.

During the Global Financial Crisis of 2007-2008, the collapse of subprime residential mortgage-backed securities (RMBS) and associated collateralized debt obligations (CDOs) catalyzed widespread market illiquidity. The crisis revealed systemic vulnerabilities including:
1. Shadow Banking System Fragility: Non-bank financial intermediaries operating outside traditional regulatory oversight utilized asset-backed commercial paper (ABCP) conduits and bilateral repurchase agreements without access to central bank standing liquidity facilities.
2. Pro-cyclical Leverage and Margin Spirals: As documented by Brunnermeier and Pedersen (2009), asset price declines induced margin calls and collateral haircut increases, compelling financial institutions to engage in asset fire-sales to maintain solvency. This triggered secondary price depreciations and self-reinforcing insolvency spirals across interconnected counterparties.
3. Network Interconnectedness and Counterparty Risk: Complex credit default swap (CDS) derivatives contracts concentrated unhedged exposures among key systemically important financial institutions (SIFIs), notably AIG and Lehman Brothers, resulting in sudden systemic gridlock upon default.
4. Liquidity Hoarding: Commercial banks ceased interbank lending in the unsecured federal funds and LIBOR markets due to informational opacity regarding counterparty asset quality, necessitating unprecedented lender-of-last-resort interventions by central banks.

In response, the Basel Committee on Banking Supervision formulated the Basel III framework, introducing the Liquidity Coverage Ratio (LCR), Net Stable Funding Ratio (NSFR), Counter-cyclical Capital Buffers (CCyB), and Global Systemically Important Bank (G-SIB) capital surcharges to mitigate procyclicality.

Please confirm receipt of this domain background and state the 4 core systemic vulnerabilities listed above in a clean numbered list.
"""


def api_request(base_url, api_key, method, path, data=None):
    url = f"{base_url.rstrip('/')}{path}"
    body = json.dumps(data).encode("utf-8") if data is not None else None
    headers = {
        "Authorization": f"Bearer {api_key}",
        "Content-Type": "application/json",
    }
    req = urllib.request.Request(url, data=body, headers=headers, method=method)
    with urllib.request.urlopen(req, timeout=120) as resp:
        res_bytes = resp.read()
        route = resp.headers.get("x-llmgateway-route", "unknown")
        parsed = json.loads(res_bytes.decode("utf-8")) if res_bytes else {}
        return resp.status, parsed, route


def create_thread(base_url, api_key, title, model):
    status, res, _ = api_request(
        base_url, api_key, "POST", "/v1/threads", {"title": title, "model": model}
    )
    return res["id"]


def delete_thread(base_url, api_key, thread_id):
    try:
        api_request(base_url, api_key, "DELETE", f"/v1/threads/{thread_id}")
    except Exception:
        pass


def send_thread_message(base_url, api_key, thread_id, model, content):
    t0 = time.perf_counter()
    status, res, route = api_request(
        base_url,
        api_key,
        "POST",
        f"/v1/threads/{thread_id}/messages",
        {"content": content, "model": model, "stream": False},
    )
    elapsed = time.perf_counter() - t0
    assistant_msg = (res.get("choices", [{}])[0].get("message", {}) or {}).get("content", "")
    return {
        "ok": status == 200 and bool(assistant_msg),
        "status_code": status,
        "elapsed_sec": elapsed,
        "route": route,
        "content": assistant_msg,
        "content_len": len(assistant_msg),
    }


def get_thread_affinity(base_url, api_key, thread_id, account_id="gemini-web-e68f3020"):
    try:
        status, res, _ = api_request(
            base_url,
            api_key,
            "GET",
            f"/_llmgateway/threads/{thread_id}/browser-affinity/{account_id}",
        )
        return res
    except Exception as e:
        return {"error": str(e)}


def get_runtime_status(base_url, api_key, account_id="gemini-web-e68f3020"):
    try:
        status, res, _ = api_request(
            base_url, api_key, "GET", f"/_llmgateway/browser-accounts/{account_id}/runtime"
        )
        return res
    except Exception as e:
        return {"error": str(e)}


def main():
    parser = argparse.ArgumentParser(description="Concurrent Long-Thread Test for Gemini on LLMGateway")
    parser.add_argument("--base-url", default=DEFAULT_BASE_URL)
    parser.add_argument("--api-key", default=DEFAULT_API_KEY)
    parser.add_argument("--model", default="gemini-web/gemini-web-flash")
    parser.add_argument("--stagger", type=float, default=0.0, help="Delay in seconds between dispatching Thread A and Thread B")
    parser.add_argument("--keep-threads", action="store_true")
    args = parser.parse_args()

    print("==========================================================================")
    print(" CONCURRENT LONG-THREAD MULTI-TURN STRESS TEST")
    print(f" Gateway URL: {args.base_url}")
    print(f" Model Under Test: {args.model}")
    print(f" Timestamp: {datetime.datetime.now().isoformat()}")
    print("==========================================================================")

    runtime_init = get_runtime_status(args.base_url, args.api_key)
    print(f"Initial Gateway Transport: {runtime_init.get('effective_transport')} | Direct Ready: {runtime_init.get('direct_ready')} | Chromium: {runtime_init.get('browser_running')}")

    # 1. Initialize 2 separate threads
    print("\n--- [STEP 1] CREATING 2 DISTINCT THREADS ---")
    thread_a = create_thread(args.base_url, args.api_key, "Thread A: Quantum Cryptography", args.model)
    thread_b = create_thread(args.base_url, args.api_key, "Thread B: Macroeconomics & Crisis", args.model)
    print(f"  Thread A created: {thread_a}")
    print(f"  Thread B created: {thread_b}")

    results = {
        "model": args.model,
        "threads": {"thread_a": thread_a, "thread_b": thread_b},
        "turns": [],
    }

    try:
        # =====================================================================
        # TURN 1: SIMULTANEOUS LONG CONTEXT INGESTION
        # =====================================================================
        print("\n--- [TURN 1] SIMULTANEOUS LONG CONTEXT INGESTION (~1200 words each) ---")
        t_turn1_start = time.perf_counter()
        with concurrent.futures.ThreadPoolExecutor(max_workers=2) as executor:
            fut_a = executor.submit(send_thread_message, args.base_url, args.api_key, thread_a, args.model, THREAD_A_CONTEXT)
            if args.stagger > 0:
                time.sleep(args.stagger)
            fut_b = executor.submit(send_thread_message, args.base_url, args.api_key, thread_b, args.model, THREAD_B_CONTEXT)
            res_a_t1 = fut_a.result()
            res_b_t1 = fut_b.result()
        t_turn1_wall = time.perf_counter() - t_turn1_start

        print(f"  [Thread A] Latency: {res_a_t1['elapsed_sec']:.2f}s | Length: {res_a_t1['content_len']} chars | Route: {res_a_t1['route']}")
        print(f"  [Thread B] Latency: {res_b_t1['elapsed_sec']:.2f}s | Length: {res_b_t1['content_len']} chars | Route: {res_b_t1['route']}")
        print(f"  Turn 1 Total Concurrent Wall Time: {t_turn1_wall:.2f}s")

        # =====================================================================
        # TURN 2: SIMULTANEOUS REASONING & MEMORY ISOLATION CHECK
        # =====================================================================
        print("\n--- [TURN 2] SIMULTANEOUS REASONING & MEMORY / CONTEXT ISOLATION CHECK ---")
        prompt_a_t2 = (
            "Please perform two tasks:\n"
            "1. State the exact SECRET IDENTIFIER given in your previous domain document.\n"
            "2. Provide a rigorous technical analysis comparing lattice-based cryptography against classical RSA in terms of key size, computational efficiency, and quantum resistance."
        )
        prompt_b_t2 = (
            "Please perform two tasks:\n"
            "1. State the exact SECRET IDENTIFIER given in your previous domain document.\n"
            "2. Provide a rigorous economic analysis explaining how Basel III Counter-cyclical Capital Buffers (CCyB) mitigate procyclical leverage spirals."
        )

        t_turn2_start = time.perf_counter()
        with concurrent.futures.ThreadPoolExecutor(max_workers=2) as executor:
            fut_a = executor.submit(send_thread_message, args.base_url, args.api_key, thread_a, args.model, prompt_a_t2)
            if args.stagger > 0:
                time.sleep(args.stagger)
            fut_b = executor.submit(send_thread_message, args.base_url, args.api_key, thread_b, args.model, prompt_b_t2)
            res_a_t2 = fut_a.result()
            res_b_t2 = fut_b.result()
        t_turn2_wall = time.perf_counter() - t_turn2_start

        # Check memory isolation
        secret_a_found = "SECRET_KEY_QUANTUM_77889" in res_a_t2["content"]
        secret_b_in_a = "SECRET_KEY_MACRO_33441" in res_a_t2["content"]

        secret_b_found = "SECRET_KEY_MACRO_33441" in res_b_t2["content"]
        secret_a_in_b = "SECRET_KEY_QUANTUM_77889" in res_b_t2["content"]

        isolation_ok = secret_a_found and secret_b_found and not secret_b_in_a and not secret_a_in_b

        print(f"  [Thread A] Latency: {res_a_t2['elapsed_sec']:.2f}s | Recalled Secret A: {secret_a_found} | Contaminated with Secret B: {secret_b_in_a}")
        print(f"  [Thread B] Latency: {res_b_t2['elapsed_sec']:.2f}s | Recalled Secret B: {secret_b_found} | Contaminated with Secret A: {secret_a_in_b}")
        if not secret_b_found:
            print(f"    [Thread B Raw Snippet]: {res_b_t2['content'][:200]}")
        print(f"  Memory Isolation Status: {'PERFECT (ZERO CROSS-TALK)' if isolation_ok else 'FAILED'}")
        print(f"  Turn 2 Total Concurrent Wall Time: {t_turn2_wall:.2f}s")

        # =====================================================================
        # TURN 3: SIMULTANEOUS COMPLEX CODE SYNTHESIS
        # =====================================================================
        print("\n--- [TURN 3] SIMULTANEOUS COMPLEX PYTHON CODE SYNTHESIS ---")
        prompt_a_t3 = (
            "Write a complete, production-grade Python script implementing a Toy Lamport One-Time Signature (OTS) scheme using hashlib.sha256.\n"
            "Include a class `LamportOTS` with `generate_keypair()`, `sign(message_bytes)`, and `verify(message_bytes, signature, public_key)`.\n"
            "Include an executable test block that generates keys, signs a message, verifies valid signature (assert True), and verifies tampered signature (assert False)."
        )
        prompt_b_t3 = (
            "Write a complete, production-grade Python script implementing a Monte Carlo Bank Stress Simulator.\n"
            "Include a class `BankStressSimulator` with methods `simulate_paths(n_trials, n_quarters)`, `calculate_capital_adequacy_ratio()`, and `compute_value_at_risk(confidence=0.99)`.\n"
            "Include an executable test block initializing sample bank assets and running a 1,000-trial simulation."
        )

        t_turn3_start = time.perf_counter()
        with concurrent.futures.ThreadPoolExecutor(max_workers=2) as executor:
            fut_a = executor.submit(send_thread_message, args.base_url, args.api_key, thread_a, args.model, prompt_a_t3)
            if args.stagger > 0:
                time.sleep(args.stagger)
            fut_b = executor.submit(send_thread_message, args.base_url, args.api_key, thread_b, args.model, prompt_b_t3)
            res_a_t3 = fut_a.result()
            res_b_t3 = fut_b.result()
        t_turn3_wall = time.perf_counter() - t_turn3_start

        # Validate code syntax
        def check_python_syntax(text):
            code_blocks = re.findall(r"```(?:python)?\s*(.*?)```", text, re.DOTALL)
            code = code_blocks[0] if code_blocks else text
            try:
                compile(code, "<string>", "exec")
                return True, len(code.splitlines())
            except Exception as e:
                return False, f"{e} (raw start: {code[:80]!r})"

        a_code_ok, a_lines = check_python_syntax(res_a_t3["content"])
        b_code_ok, b_lines = check_python_syntax(res_b_t3["content"])

        print(f"  [Thread A Code] Latency: {res_a_t3['elapsed_sec']:.2f}s | Valid Syntax: {a_code_ok} ({a_lines} lines)")
        print(f"  [Thread B Code] Latency: {res_b_t3['elapsed_sec']:.2f}s | Valid Syntax: {b_code_ok} ({b_lines} lines)")
        print(f"  Turn 3 Total Concurrent Wall Time: {t_turn3_wall:.2f}s")

        # =====================================================================
        # AFFINITY & RUNTIME TELEMETRY VERIFICATION
        # =====================================================================
        print("\n--- [STEP 4] NATIVE GEMINI AFFINITY & RUNTIME TELEMETRY ---")
        affinity_a = get_thread_affinity(args.base_url, args.api_key, thread_a)
        affinity_b = get_thread_affinity(args.base_url, args.api_key, thread_b)
        runtime_post = get_runtime_status(args.base_url, args.api_key)

        url_a = affinity_a.get("mapping", {}).get("conversation_url", "none")
        url_b = affinity_b.get("mapping", {}).get("conversation_url", "none")
        ord_a = affinity_a.get("mapping", {}).get("last_synced_ordinal", -1)
        ord_b = affinity_b.get("mapping", {}).get("last_synced_ordinal", -1)

        distinct_conversations = (url_a != url_b) and (url_a != "none")
        chromium_running = runtime_post.get("browser_running", True)
        direct_ready = runtime_post.get("direct_ready", False)

        print(f"  Thread A Gemini Conversation: {url_a} (Ordinal: {ord_a})")
        print(f"  Thread B Gemini Conversation: {url_b} (Ordinal: {ord_b})")
        print(f"  Distinct Native Conversations: {distinct_conversations}")
        print(f"  Chromium Running: {chromium_running}")
        print(f"  Direct Ready: {direct_ready}")

        # Summary structure
        summary = {
            "model": args.model,
            "turns": [
                {
                    "turn": 1,
                    "wall_time_sec": t_turn1_wall,
                    "a": {"latency": res_a_t1["elapsed_sec"], "chars": res_a_t1["content_len"], "content": res_a_t1["content"]},
                    "b": {"latency": res_b_t1["elapsed_sec"], "chars": res_b_t1["content_len"], "content": res_b_t1["content"]},
                },
                {
                    "turn": 2,
                    "wall_time_sec": t_turn2_wall,
                    "a": {"latency": res_a_t2["elapsed_sec"], "chars": res_a_t2["content_len"], "recalled": secret_a_found, "content": res_a_t2["content"]},
                    "b": {"latency": res_b_t2["elapsed_sec"], "chars": res_b_t2["content_len"], "recalled": secret_b_found, "content": res_b_t2["content"]},
                    "isolated": isolation_ok,
                },
                {
                    "turn": 3,
                    "wall_time_sec": t_turn3_wall,
                    "a": {"latency": res_a_t3["elapsed_sec"], "code_ok": a_code_ok, "lines": a_lines, "content": res_a_t3["content"]},
                    "b": {"latency": res_b_t3["elapsed_sec"], "code_ok": b_code_ok, "lines": b_lines, "content": res_b_t3["content"]},
                },
            ],
            "affinity": {
                "distinct_conversations": distinct_conversations,
                "url_a": url_a,
                "url_b": url_b,
                "ord_a": ord_a,
                "ord_b": ord_b,
            },
            "runtime": {
                "direct_ready": direct_ready,
                "chromium_running": chromium_running,
                "effective_transport": runtime_post.get("effective_transport"),
            }
        }

        with open("scripts/concurrent_threads_result.json", "w", encoding="utf-8") as f:
            json.dump(summary, f, indent=2)
        print(f"\nDetailed JSON saved to scripts/concurrent_threads_result.json")

    finally:
        if not args.keep_threads:
            print("\nCleaning up test threads...")
            delete_thread(args.base_url, args.api_key, thread_a)
            delete_thread(args.base_url, args.api_key, thread_b)
            print("Threads deleted successfully.")
        else:
            print(f"\nPreserved threads: {thread_a}, {thread_b}")


if __name__ == "__main__":
    main()
