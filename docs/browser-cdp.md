# Browser CDP adapter (v0.16+)

`browser-cdp` executes a small local adapter script inside an already-authenticated Chromium page through the loopback Chrome DevTools Protocol (CDP).

The design keeps authentication material inside the isolated Chromium profile:

- llmgateway does not export or persist raw cookies.
- the browser session must already be `ready` before the route is eligible.
- CAPTCHA and 2FA are completed normally by the user in Chromium.
- the adapter script runs in the page context, so normal same-origin browser credentials remain owned by Chromium.
- CDP remains reachable only through the loopback debugger started by the Chromium driver.

This is an experimental integration surface. Provider-specific scripts must respect the provider's terms and must not be used to bypass authentication challenges, anti-abuse controls, or provider quota enforcement.

## Configuration

A CDP-backed account uses the same routing model as every other llmgateway account. Since v0.17, browser accounts are first-class and do not need dummy API credentials or an explicit `discover_models = false`.

```toml
[browser]
enabled = true
profile_root = "data/browser-profiles"

[browser.sessions.example-web]
provider = "example-web"
label = "Example web account"
login_url = "https://example.com/chat"
enabled = true

[browser.bindings.example-web-account]
session = "example-web"
target_url_prefix = "https://example.com/chat"
adapter_script = "adapters/example.js"

[chromium]
enabled = true
startup_timeout_seconds = 15

[chromium.sessions.example-web]
enabled = true
ready_url_prefixes = ["https://example.com/chat"]

[[providers]]
id = "example-web"
kind = "browser-cdp"

[[accounts]]
id = "example-web-account"
provider = "example-web"
enabled = true

[[routes]]
id = "example-web-route"
account = "example-web-account"
model = "example-model"
priority = 10
enabled = true
capabilities = ["chat"]
```

`target_url_prefix` chooses which authenticated page target receives the CDP call. `adapter_script` is a local trusted JavaScript file. The core limits adapter scripts to 512 KiB.

## Script contract

The script must expose:

```js
globalThis.__LLMGATEWAY_ADAPTER__ = {
  async chat(request) {
    return {
      status: 200,
      content_type: "application/json",
      body: {
        id: "chatcmpl_example",
        object: "chat.completion",
        model: request.model,
        choices: [
          {
            index: 0,
            message: { role: "assistant", content: "hello" },
            finish_reason: "stop"
          }
        ]
      }
    };
  }
};
```

The request passed to `chat()` is OpenAI-chat shaped and the physical route model has already replaced the virtual model name.

The result envelope is deliberately small:

- `status`: HTTP-style status code, default `200`.
- `content_type`: response media type, default `application/json`.
- `body`: either a JSON value or a string.

For streaming, return `content_type = "text/event-stream"` and `body` as a complete OpenAI-compatible SSE payload. v0.16/v0.17 buffers that returned string before forwarding it. True incremental CDP streaming is intentionally deferred to a later milestone.

## Failure semantics

The browser route still uses the normal Router and Gateway behavior:

- a session that is not `ready` is excluded during route planning;
- 429 participates in normal account quota/cooldown handling;
- 401/403 marks the browser session `requires_attention` and the router can fail over to another account/provider;
- transport/script failures count as route failures and can fail over according to the existing route order.

## Security boundary

Treat an adapter script as trusted local code. It runs in the authenticated page and therefore has the same origin-level authority available to page JavaScript. Keep scripts in a user-controlled local directory, review them before use, and do not accept adapter scripts from untrusted sources.
