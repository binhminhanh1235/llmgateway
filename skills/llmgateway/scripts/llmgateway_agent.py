#!/usr/bin/env python3
"""Small stdlib-only llmgateway helper for AI agents.

The CLI intentionally exposes READ and EXECUTE operations only.
Mutating admin operations must use the documented API explicitly after approval.
"""

from __future__ import annotations

import argparse
import json
import os
import sys
import urllib.error
import urllib.parse
import urllib.request
from typing import Any


DEFAULT_BASE_URL = "http://127.0.0.1:7331"


class GatewayError(RuntimeError):
    pass


class GatewayClient:
    def __init__(
        self,
        base_url: str,
        execution_key: str | None = None,
        admin_key: str | None = None,
        timeout: float = 30.0,
    ) -> None:
        self.base_url = base_url.rstrip("/")
        self.execution_key = execution_key
        self.admin_key = admin_key
        self.timeout = timeout

    def _request(
        self,
        method: str,
        path: str,
        *,
        body: dict[str, Any] | None = None,
        auth: str = "none",
        anthropic: bool = False,
    ) -> Any:
        headers = {"Accept": "application/json"}
        if body is not None:
            headers["Content-Type"] = "application/json"

        if auth == "execution":
            if not self.execution_key:
                raise GatewayError(
                    "execution key missing: set LLMGATEWAY_CLIENT_API_KEY or LLMGATEWAY_API_KEY"
                )
            if anthropic:
                headers["x-api-key"] = self.execution_key
                headers["anthropic-version"] = "2023-06-01"
            else:
                headers["Authorization"] = f"Bearer {self.execution_key}"
        elif auth == "admin":
            if not self.admin_key:
                raise GatewayError(
                    "admin key missing: set LLMGATEWAY_API_KEY for admin diagnostics"
                )
            headers["Authorization"] = f"Bearer {self.admin_key}"

        payload = None if body is None else json.dumps(body).encode("utf-8")
        request = urllib.request.Request(
            self.base_url + path,
            data=payload,
            headers=headers,
            method=method,
        )
        try:
            with urllib.request.urlopen(request, timeout=self.timeout) as response:
                data = response.read()
                if not data:
                    return None
                content_type = response.headers.get("Content-Type", "")
                if "json" in content_type:
                    return json.loads(data.decode("utf-8"))
                text = data.decode("utf-8", errors="replace")
                try:
                    return json.loads(text)
                except json.JSONDecodeError:
                    return {"text": text}
        except urllib.error.HTTPError as exc:
            detail = exc.read().decode("utf-8", errors="replace")
            raise GatewayError(f"HTTP {exc.code} {path}: {detail}") from exc
        except urllib.error.URLError as exc:
            raise GatewayError(f"request failed for {path}: {exc.reason}") from exc

    def health(self) -> Any:
        return self._request("GET", "/_llmgateway/health")

    def models(self) -> Any:
        return self._request("GET", "/v1/models", auth="execution")

    def admin_models(self) -> Any:
        return self._request("GET", "/_llmgateway/models", auth="admin")

    def accounts(self) -> Any:
        return self._request("GET", "/_llmgateway/accounts", auth="admin")

    def groups(self) -> Any:
        return self._request("GET", "/_llmgateway/model-groups", auth="admin")

    def clients(self) -> Any:
        return self._request("GET", "/_llmgateway/clients", auth="admin")

    def executions(self, request_id: str | None = None) -> Any:
        path = "/_llmgateway/executions"
        if request_id:
            path += "/" + urllib.parse.quote(request_id, safe="")
        return self._request("GET", path, auth="admin")

    def agent_capabilities(self) -> Any:
        return self._request(
            "GET", "/_llmgateway/agent/capabilities", auth="execution"
        )

    def agent_resolve(
        self,
        *,
        model: str | None = None,
        task: str | None = None,
        capabilities: list[str] | None = None,
        min_context_window: int | None = None,
        prompt: str | None = None,
        diagnostics: bool = False,
    ) -> Any:
        body: dict[str, Any] = {
            "requirements": {
                "capabilities": capabilities or [],
                "min_context_window": min_context_window,
            }
        }
        if model:
            body["model"] = model
        if task:
            body["task"] = task
        if prompt:
            body["body"] = {
                "messages": [{"role": "user", "content": prompt}]
            }
        endpoint = (
            "/_llmgateway/agent/diagnostics"
            if diagnostics
            else "/_llmgateway/agent/resolve"
        )
        return self._request("POST", endpoint, body=body, auth="execution")

    def explain(
        self,
        model: str,
        *,
        client_id: str | None = None,
        prompt: str | None = None,
    ) -> Any:
        body: dict[str, Any] = {"model": model}
        if client_id:
            body["client_id"] = client_id
        if prompt:
            body["body"] = {
                "messages": [{"role": "user", "content": prompt}]
            }
        return self._request(
            "POST", "/_llmgateway/routes/explain", body=body, auth="admin"
        )

    @staticmethod
    def _routing_hints(
        body: dict[str, Any],
        *,
        task: str | None = None,
        capabilities: list[str] | None = None,
        min_context_window: int | None = None,
    ) -> dict[str, Any]:
        if task:
            body["llmgateway_task"] = task
        if capabilities or min_context_window is not None:
            body["llmgateway_requirements"] = {
                "capabilities": capabilities or [],
                "min_context_window": min_context_window,
            }
        return body

    def responses(
        self,
        model: str,
        prompt: str,
        *,
        task: str | None = None,
        capabilities: list[str] | None = None,
        min_context_window: int | None = None,
    ) -> Any:
        body = self._routing_hints(
            {"model": model, "input": prompt},
            task=task,
            capabilities=capabilities,
            min_context_window=min_context_window,
        )
        return self._request(
            "POST",
            "/v1/responses",
            body=body,
            auth="execution",
        )

    def chat(
        self,
        model: str,
        prompt: str,
        *,
        task: str | None = None,
        capabilities: list[str] | None = None,
        min_context_window: int | None = None,
    ) -> Any:
        body = self._routing_hints(
            {
                "model": model,
                "messages": [{"role": "user", "content": prompt}],
            },
            task=task,
            capabilities=capabilities,
            min_context_window=min_context_window,
        )
        return self._request(
            "POST",
            "/v1/chat/completions",
            body=body,
            auth="execution",
        )

    def messages(
        self,
        model: str,
        prompt: str,
        max_tokens: int,
        *,
        task: str | None = None,
        capabilities: list[str] | None = None,
        min_context_window: int | None = None,
    ) -> Any:
        body = self._routing_hints(
            {
                "model": model,
                "max_tokens": max_tokens,
                "messages": [{"role": "user", "content": prompt}],
            },
            task=task,
            capabilities=capabilities,
            min_context_window=min_context_window,
        )
        return self._request(
            "POST",
            "/v1/messages",
            body=body,
            auth="execution",
            anthropic=True,
        )


def _execution_key() -> str | None:
    return os.environ.get("LLMGATEWAY_CLIENT_API_KEY") or os.environ.get(
        "LLMGATEWAY_API_KEY"
    )


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description="Safe llmgateway helper for AI agents")
    parser.add_argument(
        "--base-url",
        default=os.environ.get("LLMGATEWAY_BASE_URL", DEFAULT_BASE_URL),
    )
    parser.add_argument("--timeout", type=float, default=30.0)
    sub = parser.add_subparsers(dest="command", required=True)

    sub.add_parser("health")
    sub.add_parser("models")
    sub.add_parser("capabilities")

    resolve = sub.add_parser("resolve")
    resolve.add_argument("--model")
    resolve.add_argument("--task")
    resolve.add_argument("--capability", action="append", default=[])
    resolve.add_argument("--min-context-window", type=int)
    resolve.add_argument("--prompt")

    diagnostics = sub.add_parser("diagnostics")
    diagnostics.add_argument("--model")
    diagnostics.add_argument("--task")
    diagnostics.add_argument("--capability", action="append", default=[])
    diagnostics.add_argument("--min-context-window", type=int)
    diagnostics.add_argument("--prompt")
    sub.add_parser("admin-models")
    sub.add_parser("accounts")
    sub.add_parser("groups")
    sub.add_parser("clients")

    executions = sub.add_parser("executions")
    executions.add_argument("--request-id")

    explain = sub.add_parser("explain")
    explain.add_argument("model")
    explain.add_argument("--client-id")
    explain.add_argument("--prompt")

    def add_routing_arguments(command: argparse.ArgumentParser) -> None:
        command.add_argument("--task")
        command.add_argument("--capability", action="append", default=[])
        command.add_argument("--min-context-window", type=int)

    responses = sub.add_parser("responses")
    responses.add_argument("model")
    responses.add_argument("prompt")
    add_routing_arguments(responses)

    chat = sub.add_parser("chat")
    chat.add_argument("model")
    chat.add_argument("prompt")
    add_routing_arguments(chat)

    messages = sub.add_parser("messages")
    messages.add_argument("model")
    messages.add_argument("prompt")
    messages.add_argument("--max-tokens", type=int, default=1024)
    add_routing_arguments(messages)

    return parser


def run(args: argparse.Namespace) -> Any:
    client = GatewayClient(
        args.base_url,
        execution_key=_execution_key(),
        admin_key=os.environ.get("LLMGATEWAY_API_KEY"),
        timeout=args.timeout,
    )
    if args.command == "health":
        return client.health()
    if args.command == "models":
        return client.models()
    if args.command == "capabilities":
        return client.agent_capabilities()
    if args.command in {"resolve", "diagnostics"}:
        return client.agent_resolve(
            model=args.model,
            task=args.task,
            capabilities=args.capability,
            min_context_window=args.min_context_window,
            prompt=args.prompt,
            diagnostics=args.command == "diagnostics",
        )
    if args.command == "admin-models":
        return client.admin_models()
    if args.command == "accounts":
        return client.accounts()
    if args.command == "groups":
        return client.groups()
    if args.command == "clients":
        return client.clients()
    if args.command == "executions":
        return client.executions(args.request_id)
    if args.command == "explain":
        return client.explain(
            args.model, client_id=args.client_id, prompt=args.prompt
        )
    if args.command == "responses":
        return client.responses(
            args.model,
            args.prompt,
            task=args.task,
            capabilities=args.capability,
            min_context_window=args.min_context_window,
        )
    if args.command == "chat":
        return client.chat(
            args.model,
            args.prompt,
            task=args.task,
            capabilities=args.capability,
            min_context_window=args.min_context_window,
        )
    if args.command == "messages":
        return client.messages(
            args.model,
            args.prompt,
            args.max_tokens,
            task=args.task,
            capabilities=args.capability,
            min_context_window=args.min_context_window,
        )
    raise GatewayError(f"unsupported command: {args.command}")


def main() -> int:
    args = build_parser().parse_args()
    try:
        result = run(args)
    except GatewayError as exc:
        print(str(exc), file=sys.stderr)
        return 1
    print(json.dumps(result, indent=2, ensure_ascii=False))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
