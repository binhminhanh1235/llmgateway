#!/usr/bin/env python3
import json
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *_args):
        return

    def do_POST(self):
        length = int(self.headers.get("content-length", "0"))
        body = json.loads(self.rfile.read(length) or b"{}")
        messages = body.get("messages", [])
        system_text = "\n".join(
            str(message.get("content", ""))
            for message in messages
            if message.get("role") == "system"
        )
        if "structured memory compiler" in system_text.lower():
            text = json.dumps({
                "facts": ["The smoke thread has durable context"],
                "decisions": ["Use structured memory snapshots"],
                "constraints": ["Keep the full transcript in SQLite"],
                "user_preferences": ["Prefer local-first behavior"],
                "entities": ["llmgateway", "ContextEngine"],
                "code_context": ["thread_memories stores schema-versioned JSON"],
                "open_questions": ["What should v0.7 optimize next?"],
                "rolling_summary": "The thread is validating structured memory compaction end to end."
            })
        else:
            retrieved = "retrieved earlier transcript excerpts" in system_text.lower()
            text = f"fake reply messages={len(messages)} retrieval={'yes' if retrieved else 'no'}"

        if body.get("stream"):
            self.send_response(200)
            self.send_header("content-type", "text/event-stream")
            self.end_headers()
            chunk = {
                "id": "chatcmpl_fake",
                "object": "chat.completion.chunk",
                "choices": [{"index": 0, "delta": {"content": text}, "finish_reason": None}],
            }
            self.wfile.write(f"data: {json.dumps(chunk)}\n\n".encode())
            done = {
                "id": "chatcmpl_fake",
                "object": "chat.completion.chunk",
                "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": len(messages), "completion_tokens": 3},
            }
            self.wfile.write(f"data: {json.dumps(done)}\n\n".encode())
            self.wfile.write(b"data: [DONE]\n\n")
            self.wfile.flush()
            return

        response = {
            "id": "chatcmpl_fake",
            "object": "chat.completion",
            "model": body.get("model", "fake-model"),
            "choices": [{
                "index": 0,
                "finish_reason": "stop",
                "message": {"role": "assistant", "content": text},
            }],
            "usage": {
                "prompt_tokens": len(messages),
                "completion_tokens": 3,
                "total_tokens": len(messages) + 3,
            },
        }
        payload = json.dumps(response).encode()
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)


if __name__ == "__main__":
    ThreadingHTTPServer(("127.0.0.1", 18080), Handler).serve_forever()
