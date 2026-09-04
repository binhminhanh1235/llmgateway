#!/usr/bin/env python3
import base64
import hashlib
import json
import os
import re
import socketserver
import struct
import sys
import threading
import time
from urllib.parse import urlparse

profile = ""
login_url = "https://chat.qwen.ai/"
for arg in sys.argv[1:]:
    if arg.startswith("--user-data-dir="):
        profile = arg.split("=", 1)[1]
    elif arg.startswith("http://") or arg.startswith("https://"):
        login_url = arg

if not profile:
    raise SystemExit("missing --user-data-dir")

os.makedirs(profile, exist_ok=True)
page_url = (
    "https://gemini.google.com/app"
    if "gemini.google.com" in login_url
    else "https://chat.qwen.ai/"
)

stream_lock = threading.Lock()
stream_seq = 0
streams = {}

def expression_request(expression):
    match = re.search(r"const __request = (.*?);\n", expression, re.S)
    if not match:
        return {}
    try:
        return json.loads(match.group(1))
    except Exception:
        return {}

def write_marker(name, value="1"):
    try:
        with open(os.path.join(profile, name), "w", encoding="utf-8") as f:
            f.write(str(value))
    except Exception:
        pass

def adapter_identity(expression):
    if "gemini-web" in expression or '"provider":"gemini"' in expression:
        return "gemini-web", "gemini", "gemini-web-default"
    return "qwen-web", "qwen", "qwen-web-default"

def envelope_for(expression):
    global stream_seq
    adapter_id, provider, model = adapter_identity(expression)
    meta = {
        "contract_version": 1,
        "id": adapter_id,
        "provider": provider,
        "adapter_version": "ci-fake-cdp-v2-stream",
    }
    probe = {
        "ok": True,
        "code": "ready",
        "message": "fake authenticated page ready",
        "page_signature": f"ci-{provider}-authenticated",
    }

    if 'const __operation = "chat_stream_start"' in expression:
        with stream_lock:
            stream_seq += 1
            stream_id = f"ci-stream-{stream_seq}"
            streams[stream_id] = {"poll": 0, "cancelled": False, "model": model}
        write_marker("stream-started", stream_id)
        return {
            "meta": meta,
            "probe": probe,
            "stream": {
                "stream_id": stream_id,
                "status": 200,
                "content_type": "text/event-stream",
            },
        }

    if 'const __operation = "chat_stream_poll"' in expression:
        request = expression_request(expression)
        stream_id = str(request.get("stream_id", ""))
        with stream_lock:
            state = streams.get(stream_id)
            if state is None:
                return {
                    "meta": meta,
                    "stream": {
                        "events": [],
                        "done": True,
                        "error": {"code": "stream_not_found", "message": "fake stream not found"},
                    },
                }
            if state["cancelled"]:
                return {
                    "meta": meta,
                    "stream": {
                        "events": [],
                        "done": True,
                        "error": {"code": "cancelled", "message": "fake stream cancelled"},
                    },
                }
            state["poll"] += 1
            poll = state["poll"]
            model = state["model"]

        write_marker("stream-poll-count", poll)
        completion_id = "chatcmpl_browser_stream"
        if poll == 1:
            events = [{
                "id": completion_id,
                "object": "chat.completion.chunk",
                "model": model,
                "choices": [{"index": 0, "delta": {"role": "assistant", "content": "browser-"}, "finish_reason": None}],
            }]
            return {"meta": meta, "stream": {"events": events, "done": False, "error": None}}
        if poll == 2:
            time.sleep(0.15)
            events = [{
                "id": completion_id,
                "object": "chat.completion.chunk",
                "model": model,
                "choices": [{"index": 0, "delta": {"content": "stream-"}, "finish_reason": None}],
            }]
            return {"meta": meta, "stream": {"events": events, "done": False, "error": None}}

        time.sleep(1.0)
        events = [
            {
                "id": completion_id,
                "object": "chat.completion.chunk",
                "model": model,
                "choices": [{"index": 0, "delta": {"content": "ok"}, "finish_reason": None}],
            },
            {
                "id": completion_id,
                "object": "chat.completion.chunk",
                "model": model,
                "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
            },
        ]
        with stream_lock:
            streams.pop(stream_id, None)
        write_marker("stream-completed", stream_id)
        return {"meta": meta, "stream": {"events": events, "done": True, "error": None}}

    if 'const __operation = "chat_stream_cancel"' in expression:
        request = expression_request(expression)
        stream_id = str(request.get("stream_id", ""))
        with stream_lock:
            state = streams.get(stream_id)
            if state is not None:
                state["cancelled"] = True
        write_marker("stream-cancelled", stream_id or "missing")
        return {"meta": meta, "stream": {"cancelled": state is not None}}

    if 'const __operation = "chat"' in expression:
        return {
            "meta": meta,
            "probe": probe,
            "result": {
                "status": 200,
                "content_type": "application/json",
                "body": {
                    "id": "chatcmpl_browser_hot",
                    "object": "chat.completion",
                    "model": model,
                    "choices": [{
                        "index": 0,
                        "message": {"role": "assistant", "content": "browser-hot-ok"},
                        "finish_reason": "stop",
                    }],
                    "usage": {"prompt_tokens": 4, "completion_tokens": 3, "total_tokens": 7},
                },
            },
        }
    return {"meta": meta, "probe": probe}

def read_exact(stream, size):
    data = b""
    while len(data) < size:
        chunk = stream.read(size - len(data))
        if not chunk:
            raise EOFError
        data += chunk
    return data

def read_frame(stream):
    first = read_exact(stream, 2)
    opcode = first[0] & 0x0F
    masked = bool(first[1] & 0x80)
    length = first[1] & 0x7F
    if length == 126:
        length = struct.unpack("!H", read_exact(stream, 2))[0]
    elif length == 127:
        length = struct.unpack("!Q", read_exact(stream, 8))[0]
    mask = read_exact(stream, 4) if masked else None
    payload = bytearray(read_exact(stream, length))
    if mask:
        for i in range(len(payload)):
            payload[i] ^= mask[i % 4]
    return opcode, bytes(payload)

def send_frame(stream, opcode, payload):
    if isinstance(payload, str):
        payload = payload.encode()
    header = bytearray([0x80 | opcode])
    size = len(payload)
    if size < 126:
        header.append(size)
    elif size <= 0xFFFF:
        header.append(126)
        header.extend(struct.pack("!H", size))
    else:
        header.append(127)
        header.extend(struct.pack("!Q", size))
    stream.write(bytes(header) + payload)
    stream.flush()

def target(port, target_id="fake-page"):
    return {
        "id": target_id,
        "type": "page",
        "url": page_url,
        "webSocketDebuggerUrl": f"ws://127.0.0.1:{port}/devtools/page/{target_id}",
    }

class Handler(socketserver.StreamRequestHandler):
    def handle(self):
        request_line = self.rfile.readline().decode("latin1").strip()
        if not request_line:
            return
        parts = request_line.split(" ")
        if len(parts) < 2:
            return
        method, path = parts[0], parts[1]
        headers = {}
        while True:
            line = self.rfile.readline().decode("latin1")
            if line in ("\r\n", "\n", ""):
                break
            key, _, value = line.partition(":")
            headers[key.strip().lower()] = value.strip()

        if headers.get("upgrade", "").lower() == "websocket":
            key = headers.get("sec-websocket-key", "")
            accept = base64.b64encode(
                hashlib.sha1((key + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").encode()).digest()
            ).decode()
            self.wfile.write(
                (
                    "HTTP/1.1 101 Switching Protocols\r\n"
                    "Upgrade: websocket\r\n"
                    "Connection: Upgrade\r\n"
                    f"Sec-WebSocket-Accept: {accept}\r\n\r\n"
                ).encode()
            )
            self.wfile.flush()
            try:
                while True:
                    opcode, payload = read_frame(self.rfile)
                    if opcode == 0x8:
                        send_frame(self.wfile, 0x8, b"")
                        return
                    if opcode == 0x9:
                        send_frame(self.wfile, 0xA, payload)
                        continue
                    if opcode != 0x1:
                        continue
                    command = json.loads(payload.decode())
                    if command.get("method") != "Runtime.evaluate":
                        continue
                    expression = command.get("params", {}).get("expression", "")
                    response = {
                        "id": command.get("id"),
                        "result": {
                            "result": {
                                "type": "object",
                                "value": envelope_for(expression),
                            }
                        },
                    }
                    send_frame(self.wfile, 0x1, json.dumps(response))
            except (EOFError, ConnectionError, BrokenPipeError):
                return

        port = self.server.server_address[1]
        parsed = urlparse(path)
        if method == "GET" and parsed.path == "/json/list":
            body = json.dumps([target(port)]).encode()
            self.http_response(200, body, "application/json")
        elif method == "PUT" and parsed.path == "/json/new":
            body = json.dumps(target(port, "fake-ephemeral")).encode()
            self.http_response(200, body, "application/json")
        elif method == "GET" and parsed.path.startswith("/json/close/"):
            write_marker("target-closed", parsed.path.rsplit("/", 1)[-1])
            self.http_response(200, b"Target is closing", "text/plain")
        else:
            self.http_response(404, b"", "text/plain")

    def http_response(self, status, body, content_type):
        reason = "OK" if status == 200 else "Not Found"
        self.wfile.write(
            (
                f"HTTP/1.1 {status} {reason}\r\n"
                f"Content-Type: {content_type}\r\n"
                f"Content-Length: {len(body)}\r\n"
                "Connection: close\r\n\r\n"
            ).encode()
            + body
        )
        self.wfile.flush()

class Server(socketserver.ThreadingTCPServer):
    allow_reuse_address = True
    daemon_threads = True

with Server(("127.0.0.1", 0), Handler) as server:
    port = server.server_address[1]
    with open(os.path.join(profile, "DevToolsActivePort"), "w", encoding="utf-8") as f:
        f.write(f"{port}\n/devtools/browser/fake\n")
    server.serve_forever()
