#!/usr/bin/env python3
import json
import os
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

EMBED_LOG = os.environ.get("FAKE_EMBED_LOG", "/tmp/llmgateway-fake-embeddings.log")


def embedding_for(text):
    text = str(text).lower()
    if any(term in text for term in [
        "optimistic", "version column", "conflict", "invoice", "payment update",
        "lost update", "lost updates", "concurrency control", "concurrent write",
    ]):
        return [1.0, 0.0, 0.0, 0.0]
    if any(term in text for term in ["kafka", "sendfile", "zero copy", "kernel copy"]):
        return [0.0, 1.0, 0.0, 0.0]
    if any(term in text for term in ["deploy", "release", "container rollout"]):
        return [0.0, 0.0, 1.0, 0.0]
    return [0.0, 0.0, 0.0, 1.0]


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *_args):
        return

    def do_POST(self):
        length = int(self.headers.get("content-length", "0"))
        body = json.loads(self.rfile.read(length) or b"{}")

        if self.path.rstrip("/").endswith("/embeddings"):
            inputs = body.get("input", [])
            if isinstance(inputs, str):
                inputs = [inputs]
            with open(EMBED_LOG, "a", encoding="utf-8") as log:
                log.write(json.dumps({"model": body.get("model"), "count": len(inputs)}) + "\n")
            response = {
                "object": "list",
                "model": body.get("model", "fake-embedding"),
                "data": [
                    {"object": "embedding", "index": index, "embedding": embedding_for(text)}
                    for index, text in enumerate(inputs)
                ],
                "usage": {"prompt_tokens": len(inputs), "total_tokens": len(inputs)},
            }
            payload = json.dumps(response).encode()
            self.send_response(200)
            self.send_header("content-type", "application/json")
            self.send_header("content-length", str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)
            return

        auth = self.headers.get("authorization", "")
        if "slow200" in auth:
            time.sleep(0.15)
        if "quota429" in auth:
            payload = json.dumps({
                "error": {
                    "type": "rate_limit_error",
                    "message": "fake quota exhausted for primary account",
                }
            }).encode()
            self.send_response(429)
            self.send_header("content-type", "application/json")
            self.send_header("retry-after", "120")
            self.send_header("x-ratelimit-remaining-requests", "0")
            self.send_header("x-ratelimit-remaining-tokens", "0")
            self.send_header("content-length", str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)
            return

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
            text = f"fake reply messages={len(messages)}"
            if "retrieved earlier transcript excerpts" in system_text.lower():
                text += " retrieval=yes"
            if "pinned llmgateway memory" in system_text.lower():
                text += " pin=yes"

        if body.get("stream"):
            self.send_response(200)
            self.send_header("content-type", "text/event-stream")
            self.send_header("x-ratelimit-remaining-requests", "99")
            self.send_header("x-ratelimit-remaining-tokens", "9999")
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
        self.send_header("x-ratelimit-remaining-requests", "99")
        self.send_header("x-ratelimit-remaining-tokens", "9999")
        self.send_header("content-length", str(len(payload)))
        self.end_headers()
        self.wfile.write(payload)


if __name__ == "__main__":
    ThreadingHTTPServer(("127.0.0.1", 18080), Handler).serve_forever()
