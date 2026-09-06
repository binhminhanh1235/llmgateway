from __future__ import annotations

import importlib.util
import json
import os
import pathlib
import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


SCRIPT = pathlib.Path(__file__).resolve().parents[1] / "scripts" / "llmgateway_agent.py"
SPEC = importlib.util.spec_from_file_location("llmgateway_agent", SCRIPT)
MODULE = importlib.util.module_from_spec(SPEC)
assert SPEC.loader is not None
SPEC.loader.exec_module(MODULE)


class Handler(BaseHTTPRequestHandler):
    requests = []

    def log_message(self, format, *args):
        pass

    def _record(self):
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
        payload = {"ok": True, "path": self.path}
        data = json.dumps(payload).encode("utf-8")
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)

    do_GET = _record
    do_POST = _record


class AgentHelperTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        cls.thread = threading.Thread(target=cls.server.serve_forever, daemon=True)
        cls.thread.start()
        cls.base_url = f"http://127.0.0.1:{cls.server.server_address[1]}"

    @classmethod
    def tearDownClass(cls):
        cls.server.shutdown()
        cls.server.server_close()
        cls.thread.join(timeout=2)

    def setUp(self):
        Handler.requests.clear()

    def client(self):
        return MODULE.GatewayClient(
            self.base_url,
            execution_key="client-secret",
            admin_key="admin-secret",
        )

    def test_models_uses_execution_bearer_key(self):
        self.client().models()
        request = Handler.requests[-1]
        self.assertEqual(request["path"], "/v1/models")
        self.assertEqual(request["headers"]["authorization"], "Bearer client-secret")

    def test_messages_uses_anthropic_headers(self):
        self.client().messages("llmgateway-coding", "hello", 321)
        request = Handler.requests[-1]
        self.assertEqual(request["path"], "/v1/messages")
        self.assertEqual(request["headers"]["x-api-key"], "client-secret")
        self.assertEqual(request["headers"]["anthropic-version"], "2023-06-01")
        self.assertEqual(request["body"]["max_tokens"], 321)

    def test_explain_uses_admin_key_and_task_body(self):
        self.client().explain(
            "llmgateway-auto", client_id="codex", prompt="debug Rust"
        )
        request = Handler.requests[-1]
        self.assertEqual(request["path"], "/_llmgateway/routes/explain")
        self.assertEqual(request["headers"]["Authorization"], "Bearer admin-secret")
        self.assertEqual(request["body"]["client_id"], "codex")
        self.assertEqual(
            request["body"]["body"]["messages"][0]["content"], "debug Rust"
        )

    def test_helper_does_not_define_mutating_commands(self):
        parser = MODULE.build_parser()
        with self.assertRaises(SystemExit):
            parser.parse_args(["delete-account", "anything"])


if __name__ == "__main__":
    unittest.main()
