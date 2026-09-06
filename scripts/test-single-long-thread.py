#!/usr/bin/env python3
"""
Stress test for a single long chat thread with many multi-turn prompts on Gemini (LLMGateway).
Evaluates:
  - Context retention across 8 consecutive turns
  - Cumulative latency and response sizes
  - Complex constraint obedience across the entire conversation history
  - Historical recall (secret codes, exact coordinates, voltage thresholds)
  - Gateway thread sync & ordinal advancement
  - Zero-Chromium direct-http stability
"""

import argparse
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

TURNS_SPEC = [
    {
        "turn": 1,
        "name": "System Specification & Constraints Setup",
        "prompt": """
[SYSTEM ARCHITECTURE DOCUMENT: PROJECT SKYNET-FLEET]
You are the Chief Autonomous Systems Engineer. We are establishing an autonomous distributed drone fleet management platform.
Review and record these foundational technical parameters:

1. FLEET IDENTIFICATION:
   - Fleet ID: SKYNET_FLEET_ALPHA_909
   - Authentication Handshake Token: SIGMA_VECTOR_404_DELTA
   - Telemetry Protocol: MAVLink 2.0 over UDP port 14550

2. POWER SPECIFICATIONS:
   - Battery Chemistry: 4S LiPo (Nominal 14.8V, Maximum Full Charge 22.5V)
   - Mandatory Return-To-Launch (RTL) Voltage Cutoff: 14.8V (critical safety floor)
   - Nominal Discharge Rate: 0.15 V/km in standard atmospheric conditions

3. OPERATIONAL GEOFENCE BOUNDARIES (WGS84):
   - Latitude Range: [21.000000, 21.050000] North
   - Longitude Range: [105.800000, 105.860000] East
   - Maximum Permissible Altitude AGL: 120.0 meters
   - Maximum Ground Speed: 18.0 m/s

4. MANDATORY PROTOCOL RULE:
   At the very end of EVERY response you generate throughout this entire conversation, you must append the exact tracking tag:
   [FLEET_STATUS: ACTIVE]

Please confirm receipt, acknowledge all technical specifications, and summarize the 4 operational safety boundaries in a numbered list.
""",
        "eval": {
            "type": "contains_all",
            "must_contain": ["SKYNET_FLEET_ALPHA_909", "120", "14.8", "[FLEET_STATUS: ACTIVE]"],
        },
    },
    {
        "turn": 2,
        "name": "Mathematical Route Planning & Battery Calculation",
        "prompt": """
Operational Situation Update:
Drone #14 is executing an aerial mapping mission at position Latitude 21.010000, Longitude 105.810000, Altitude 80 meters.
Telemetry indicates current battery voltage is 15.4V.
The designated base recovery station is located at Latitude 21.040000, Longitude 105.850000, Altitude 0 meters.

Assume local flat-Earth conversion: 1 degree Latitude = 111.0 km, 1 degree Longitude = 103.0 km.
Calculate:
1. The ground Euclidean distance from Drone #14 to the base station in kilometers (show intermediate calculation).
2. Given the discharge rate of 0.15 V/km, what will the battery voltage be upon arrival at the base station?
3. Will the battery remain above or at the critical RTL cutoff voltage of 14.8V? Give a definitive YES or NO answer.

Remember the mandatory tracking tag at the end.
""",
        "eval": {
            "type": "contains_all",
            "must_contain": ["YES", "[FLEET_STATUS: ACTIVE]"],
        },
    },
    {
        "turn": 3,
        "name": "Geofence Breach Detection & State Machine",
        "prompt": """
Emergency Telemetry Alert received:
Drone #07 has reported position Latitude 21.055000, Longitude 105.830000, Altitude 135.0 meters.

Tasks:
1. Cross-reference this position against the operational geofence defined in Turn 1. Identify which exact parameters have breached the boundaries (specify values and thresholds).
2. Define a formal state-transition sequence for Drone #07 to safely resolve the incident (e.g., MISSION -> WARNING -> RTL -> EMERGENCY_LAND). Explain the triggers for each transition.

Remember the mandatory tracking tag at the end.
""",
        "eval": {
            "type": "contains_all",
            "must_contain": ["21.055", "135", "120", "[FLEET_STATUS: ACTIVE]"],
        },
    },
    {
        "turn": 4,
        "name": "Telemetry Data Schema & Validation Model",
        "prompt": """
Based on the parameters established in Turn 1 and Turn 3, design a comprehensive Python dataclass or Pydantic model named `DroneTelemetryPacket`.
Requirements:
1. Fields: drone_id (str), timestamp_utc (str/datetime), latitude (float), longitude (float), altitude_agl (float), battery_voltage (float), ground_speed (float).
2. Method `is_geofence_valid(self) -> bool` validating against the Turn 1 geofence box.
3. Method `requires_emergency_rtl(self) -> bool` evaluating voltage against the 14.8V cutoff.
Output the code block in ```python ... ```.

Remember the mandatory tracking tag at the end.
""",
        "eval": {
            "type": "python_syntax",
            "class_name": "DroneTelemetryPacket",
        },
    },
    {
        "turn": 5,
        "name": "Core Fleet Manager Implementation",
        "prompt": """
Now write a complete Python class `DroneFleetManager` that manages an in-memory fleet of drones.
Requirements:
1. Maintain an internal dictionary `self.drones` mapping drone_id to its latest telemetry packet.
2. Method `process_telemetry(packet: DroneTelemetryPacket) -> dict` returning status {'status': 'OK' | 'WARNING' | 'RTL', 'reasons': []}.
3. Method `get_fleet_health_summary() -> dict` returning total drones, active, and in-emergency counts.
4. Include an executable demonstration block under `if __name__ == '__main__':` creating Drone #14 (normal) and Drone #07 (breached) and printing their health summary.

Output clean executable Python code in ```python ... ```.
Remember the mandatory tracking tag at the end.
""",
        "eval": {
            "type": "python_syntax",
            "class_name": "DroneFleetManager",
        },
    },
    {
        "turn": 6,
        "name": "AsyncIO UDP Server Refactoring",
        "prompt": """
Refactor the architecture to support real-time network ingestion.
Write an asynchronous UDP server using Python's `asyncio.DatagramProtocol` that:
1. Listens on the MAVLink telemetry port specified in Turn 1 (14550).
2. Decodes incoming JSON datagrams into `DroneTelemetryPacket`.
3. Passes packets to an instance of `DroneFleetManager`.
4. Logs an alert whenever an RTL or geofence violation occurs.
Provide the complete runnable code snippet with `asyncio.run()`.

Remember the mandatory tracking tag at the end.
""",
        "eval": {
            "type": "python_syntax",
            "must_contain": ["14550", "DatagramProtocol"],
        },
    },
    {
        "turn": 7,
        "name": "Long-Context Memory & Historical Recall Audit",
        "prompt": """
We are conducting an automated system audit. Without looking at external references, answer these 5 precise historical audit questions from our preceding conversation:

1. What was the exact FLEET IDENTIFICATION ID specified in Turn 1?
2. What was the exact Authentication Handshake Token specified in Turn 1?
3. In Turn 2, what was the initial battery voltage and altitude of Drone #14?
4. In Turn 3, what were the exact Latitude and Altitude reported by Drone #07 that breached the boundary?
5. What is the mandatory tracking tag that must appear at the end of every message?

Answer each in a clear numbered list.
""",
        "eval": {
            "type": "contains_all",
            "must_contain": [
                "SKYNET_FLEET_ALPHA_909",
                "SIGMA_VECTOR_404_DELTA",
                "15.4",
                "80",
                "21.055",
                "135",
                "[FLEET_STATUS: ACTIVE]",
            ],
        },
    },
    {
        "turn": 8,
        "name": "Executive Architecture Synthesis & Deployment Guide",
        "prompt": """
Synthesize our entire 8-turn conversation into an executive technical deployment brief for the SkyNet-Fleet system.
Structure the brief into:
1. System Overview & Core Specifications (Fleet ID, port, limits)
2. Geofence & Power Management Protocols
3. Network & Software Architecture (AsyncIO UDP + Fleet Manager)
4. Incident Response & Failsafe Matrix

Ensure all parameters match what we established. Conclude with the mandatory tracking tag.
""",
        "eval": {
            "type": "contains_all",
            "must_contain": ["SKYNET_FLEET_ALPHA_909", "14550", "14.8", "[FLEET_STATUS: ACTIVE]"],
        },
    },
]


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


def evaluate_turn_result(turn_spec, content):
    ev = turn_spec["eval"]
    text = content.strip()
    ev_type = ev.get("type")

    if ev_type == "contains_all":
        missing = [item for item in ev["must_contain"] if item not in text]
        if not missing:
            return True, "Passed: All expected keywords and tags found"
        return False, f"Missing: {missing}"

    elif ev_type == "python_syntax":
        code_blocks = re.findall(r"```(?:python)?\s*(.*?)```", text, re.DOTALL)
        code = code_blocks[0] if code_blocks else text
        try:
            compile(code, "<string>", "exec")
            lines = len(code.splitlines())
            has_tag = "[FLEET_STATUS: ACTIVE]" in text
            tag_msg = "with tracking tag" if has_tag else "WITHOUT tracking tag"
            return True, f"Valid Python code ({lines} lines, {tag_msg})"
        except Exception as e:
            return False, f"Syntax Error: {e}"

    return True, "No specific eval"


def main():
    parser = argparse.ArgumentParser(description="Test single long thread with many prompts on Gemini")
    parser.add_argument("--base-url", default=DEFAULT_BASE_URL)
    parser.add_argument("--api-key", default=DEFAULT_API_KEY)
    parser.add_argument("--model", default="gemini-web/gemini-web-flash")
    parser.add_argument("--keep-thread", action="store_true")
    parser.add_argument("--output-json", default="scripts/single_long_thread_results.json")
    args = parser.parse_args()

    print("==========================================================================")
    print(" SINGLE LONG-THREAD DEEP MULTI-TURN CONVERSATION STRESS TEST")
    print(f" Gateway URL: {args.base_url}")
    print(f" Model: {args.model}")
    print(f" Total Turns Planned: {len(TURNS_SPEC)}")
    print(f" Timestamp: {datetime.datetime.now().isoformat()}")
    print("==========================================================================")

    # 1. Create thread
    thread_title = f"SkyNet-Fleet Autonomous Manager ({datetime.datetime.now().strftime('%H:%M:%S')})"
    thread_id = create_thread(args.base_url, args.api_key, thread_title, args.model)
    print(f"\nInitialized Thread: {thread_id} - '{thread_title}'")

    turns_results = []
    cumulative_prompt_chars = 0
    cumulative_response_chars = 0

    try:
        for turn_idx, turn_spec in enumerate(TURNS_SPEC, 1):
            t_num = turn_spec["turn"]
            t_name = turn_spec["name"]
            prompt = turn_spec["prompt"].strip()
            prompt_len = len(prompt)
            cumulative_prompt_chars += prompt_len

            print(f"\n--- [TURN {t_num}/{len(TURNS_SPEC)}] {t_name} ---")
            print(f"  Sending prompt ({prompt_len} chars | Cumulative prompt: {cumulative_prompt_chars} chars)...")

            res = send_thread_message(args.base_url, args.api_key, thread_id, args.model, prompt)
            cumulative_response_chars += res["content_len"]

            passed, eval_detail = evaluate_turn_result(turn_spec, res["content"])
            status_tag = "PASS" if passed else "FAIL"

            # Check affinity
            affinity = get_thread_affinity(args.base_url, args.api_key, thread_id)
            current_ord = affinity.get("mapping", {}).get("last_synced_ordinal", -1)
            native_url = affinity.get("mapping", {}).get("conversation_url", "none")

            print(f"  [{status_tag}] Latency: {res['elapsed_sec']:.2f}s | Response: {res['content_len']} chars | Ordinal: {current_ord}")
            print(f"  Evaluation: {eval_detail}")
            print(f"  Route: {res['route']} | Native URL: {native_url}")

            turns_results.append({
                "turn": t_num,
                "name": t_name,
                "prompt_chars": prompt_len,
                "response_chars": res["content_len"],
                "elapsed_sec": res["elapsed_sec"],
                "passed": passed,
                "eval_detail": eval_detail,
                "ordinal": current_ord,
                "native_url": native_url,
                "snippet": res["content"][:200].replace("\n", " "),
            })

            # Small pause between turns for realistic multi-turn pacing
            time.sleep(1.0)

        # Summary calculation
        total_time = sum(t["elapsed_sec"] for t in turns_results)
        passed_count = sum(1 for t in turns_results if t["passed"])
        avg_latency = total_time / len(turns_results) if turns_results else 0

        print("\n=======================================================================================")
        print(" FINAL LONG-THREAD SUMMARY")
        print("=======================================================================================")
        print(f" Total Turns: {len(turns_results)}")
        print(f" Success Rate: {passed_count}/{len(turns_results)} ({passed_count/len(turns_results)*100:.1f}%)")
        print(f" Total Elapsed Time: {total_time:.2f}s | Average Turn Latency: {avg_latency:.2f}s")
        print(f" Cumulative Content: Prompts={cumulative_prompt_chars} chars, Responses={cumulative_response_chars} chars")
        print(f" Final Conversation Ordinal: {turns_results[-1]['ordinal']}")
        print(f" Native Gemini Conversation: {turns_results[-1]['native_url']}")
        print("=======================================================================================\n")

        report = {
            "thread_id": thread_id,
            "model": args.model,
            "total_turns": len(turns_results),
            "passed_turns": passed_count,
            "total_time_sec": total_time,
            "avg_latency_sec": avg_latency,
            "cumulative_prompt_chars": cumulative_prompt_chars,
            "cumulative_response_chars": cumulative_response_chars,
            "turns": turns_results,
        }

        with open(args.output_json, "w", encoding="utf-8") as f:
            json.dump(report, f, indent=2)
        print(f"Detailed results written to {args.output_json}")

    finally:
        if not args.keep_thread:
            print(f"\nCleaning up thread {thread_id}...")
            delete_thread(args.base_url, args.api_key, thread_id)
            print("Thread deleted.")
        else:
            print(f"\nPreserved thread: {thread_id}")


if __name__ == "__main__":
    main()
