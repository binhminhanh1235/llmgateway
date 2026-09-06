from __future__ import annotations

import json
import os
import pathlib
import subprocess
import sys
import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


ROOT = pathlib.Path(__file__).resolve().parents[1]
SERVER = ROOT / "mcp" / "llmgateway_mcp.py"


class Handler(BaseHTTPRequestHandler):
    requests = []

    def log_message(self, format, *args):
        pass

    def _handle(self):
        length = int(self.headers.get("Content-Length", "0"))
        raw = self.rfile.read(length) if length else b""
        body = json.loads(raw.decode("utf-8")) if raw else None
        type(self).requests.append(
            {
                "method": self.command,
                "path": self.path,
                "headers": {key.lower(): value for key, value in self.headers.items()},
                "body": body,
            }
        )
        if self.path == "/_llmgateway/agent/capabilities":
            payload = {"object": "llmgateway.agent.capabilities", "models": []}
        elif self.path == "/_llmgateway/agent/resolve":
            payload = {
                "object": "llmgateway.agent.resolve",
                "status": "resolved",
                "selected": {"route_id": "coder-route"},
            }
        elif self.path == "/_llmgateway/agent/diagnostics":
            payload = {
                "object": "llmgateway.agent.diagnostics",
                "status": "ready",
            }
        elif self.path == "/v1/models":
            payload = {"object": "list", "data": []}
        elif self.path == "/v1/responses":
            payload = {"id": "resp_test", "output": []}
        elif self.path == "/v1/chat/completions":
            payload = {"choices": [{"message": {"role": "assistant", "content": "ok"}}]}
        elif self.path == "/v1/messages":
            payload = {"content": [{"type": "text", "text": "ok"}]}
        else:
            payload = {"ok": True}

        data = json.dumps(payload).encode("utf-8")
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    do_GET = _handle
    do_POST = _handle


class McpServerTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.http = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        cls.thread = threading.Thread(target=cls.http.serve_forever, daemon=True)
        cls.thread.start()
        env = os.environ.copy()
        env["LLMGATEWAY_BASE_URL"] = (
            f"http://127.0.0.1:{cls.http.server_address[1]}"
        )
        env["LLMGATEWAY_CLIENT_API_KEY"] = "client-secret"
        env.pop("LLMGATEWAY_API_KEY", None)
        cls.proc = subprocess.Popen(
            [sys.executable, str(SERVER)],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            bufsize=1,
            env=env,
        )

    @classmethod
    def tearDownClass(cls):
        if cls.proc.stdin:
            cls.proc.stdin.close()
        cls.proc.terminate()
        cls.proc.wait(timeout=5)
        cls.http.shutdown()
        cls.http.server_close()
        cls.thread.join(timeout=2)

    def setUp(self):
        Handler.requests.clear()

    def rpc(self, payload):
        assert self.proc.stdin is not None
        assert self.proc.stdout is not None
        self.proc.stdin.write(json.dumps(payload) + "\n")
        self.proc.stdin.flush()
        line = self.proc.stdout.readline()
        self.assertTrue(line, "MCP server exited without response")
        return json.loads(line)

    def test_modern_discovery(self):
        response = self.rpc(
            {"jsonrpc": "2.0", "id": 1, "method": "server/discover", "params": {}}
        )
        result = response["result"]
        self.assertEqual(result["resultType"], "complete")
        self.assertIn("2026-07-28", result["supportedVersions"])
        self.assertIn("tools", result["capabilities"])

    def test_legacy_initialize(self):
        response = self.rpc(
            {
                "jsonrpc": "2.0",
                "id": 2,
                "method": "initialize",
                "params": {"protocolVersion": "2025-11-25"},
            }
        )
        self.assertEqual(response["result"]["protocolVersion"], "2025-11-25")
        self.assertIn("tools", response["result"]["capabilities"])

    def test_tool_catalog_is_read_execute_only(self):
        response = self.rpc(
            {"jsonrpc": "2.0", "id": 3, "method": "tools/list", "params": {}}
        )
        names = {tool["name"] for tool in response["result"]["tools"]}
        self.assertIn("llmgateway_resolve", names)
        self.assertIn("llmgateway_chat", names)
        for forbidden in ("delete", "disable", "enable", "reset", "restart", "credential"):
            self.assertFalse(
                any(forbidden in name for name in names),
                (forbidden, names),
            )

    def test_capabilities_tool_uses_client_credential(self):
        response = self.rpc(
            {
                "jsonrpc": "2.0",
                "id": 4,
                "method": "tools/call",
                "params": {
                    "name": "llmgateway_capabilities",
                    "arguments": {},
                },
            }
        )
        self.assertFalse(response["result"]["isError"])
        request = Handler.requests[-1]
        self.assertEqual(request["path"], "/_llmgateway/agent/capabilities")
        self.assertEqual(request["headers"]["authorization"], "Bearer client-secret")

    def test_resolve_tool_forwards_semantic_requirements(self):
        response = self.rpc(
            {
                "jsonrpc": "2.0",
                "id": 5,
                "method": "tools/call",
                "params": {
                    "name": "llmgateway_resolve",
                    "arguments": {
                        "model": "llmgateway-auto",
                        "capabilities": ["coding", "reasoning"],
                        "min_context_window": 32000,
                    },
                },
            }
        )
        self.assertFalse(response["result"]["isError"])
        request = Handler.requests[-1]
        self.assertEqual(request["path"], "/_llmgateway/agent/resolve")
        self.assertEqual(
            request["body"]["requirements"]["capabilities"],
            ["coding", "reasoning"],
        )
        self.assertEqual(
            request["body"]["requirements"]["min_context_window"],
            32000,
        )


if __name__ == "__main__":
    unittest.main()
