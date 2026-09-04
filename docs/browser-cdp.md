# Custom browser CDP adapters

`browser-cdp` is the generic extension point for trusted local provider integrations.

For Gemini Web, ChatGPT Web, and Qwen Web, prefer the built-in provider kinds:

```text
browser-gemini
browser-chatgpt
browser-qwen
```

They use the same CDP contract but ship with embedded adapters, diagnostics and deterministic regression fixtures.

## Security boundary

A CDP adapter executes inside an authenticated Chromium page and therefore has the same origin-level authority available to page JavaScript.

- cookies/local storage remain inside the isolated profile;
- CDP is bound to loopback by the Chromium driver;
- raw cookies are not returned through llmgateway APIs;
- CAPTCHA, 2FA/passkeys and provider authentication remain interactive;
- adapter code must respect provider terms, anti-abuse controls and quotas.

Treat custom adapter scripts as trusted local code. Do not load scripts from untrusted sources.

## Configuration

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
adapter_script = "examples/browser-cdp-adapter.js"
adapter_contract_version = 1
ephemeral_chat = false

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

The core limits custom scripts to 512 KiB.

## Contract v1

v0.28 replaces the old implicit `chat(request)`-only contract with an explicit versioned contract.

```js
globalThis.__LLMGATEWAY_ADAPTER__ = {
  meta: {
    contract_version: 1,
    id: "example-cdp",
    provider: "example",
    adapter_version: "1.0.0"
  },

  async probe(context) {
    return {
      ok: true,
      code: "ready",
      message: "page compatible",
      page_signature: "example-v1"
    };
  },

  async chat(request, context) {
    return {
      status: 200,
      content_type: "application/json",
      body: {
        id: "chatcmpl_example",
        object: "chat.completion",
        model: request.model,
        choices: [{
          index: 0,
          message: { role: "assistant", content: "hello" },
          finish_reason: "stop"
        }]
      }
    };
  }
};
```

See `examples/browser-cdp-adapter.js` for a complete minimal adapter.

### Migration from v0.27 and earlier

A legacy custom script such as:

```js
globalThis.__LLMGATEWAY_ADAPTER__ = {
  async chat(request) { /* ... */ }
};
```

must add:

- `meta.contract_version = 1`;
- non-empty `meta.id`;
- non-empty `meta.provider`;
- a `probe(context)` function, or accept the contract's default probe behavior.

The runtime always verifies script metadata. `adapter_contract_version = 1` in TOML is an optional startup guard that makes the intended contract explicit.

A legacy script without contract metadata is reported as `browser_adapter_incompatible`; it is not silently executed as an unknown contract version.

## Probe semantics

`probe(context)` should cheaply decide whether the current authenticated page still matches the adapter.

Recommended result:

```js
{
  ok: true,
  code: "ready",
  message: "composer detected",
  page_signature: "provider-page-v3"
}
```

Common failure codes include:

```text
adapter_incompatible
login_required
wrong_page
```

The probe should not submit prompts or mutate account data.

## Chat result

`chat(request, context)` receives an OpenAI Chat Completions-shaped request whose `model` is the selected physical route model.

It returns:

- `status`: HTTP-style status, default 200;
- `content_type`: response media type, default `application/json`;
- `body`: JSON value or string.

For custom `browser-cdp` adapters that only implement `chat(request, context)`, a `text/event-stream` result remains a compatibility path and may be returned as a complete SSE string.

v0.30 adds true incremental streaming for the built-in Gemini/ChatGPT/Qwen adapters through additive contract methods:

```text
streamStart(request, context)
streamPoll({ stream_id })
streamCancel({ stream_id })
```

These methods do not change `contract_version = 1`, so existing custom v1 adapters remain compatible. Custom adapters can adopt the same stream primitives in a future contract extension; llmgateway does not pretend that an old custom buffered SSE string is incremental.

## Context object

The contract context may include:

```text
provider
adapter_id
contract_version
model_label
selectors
probe_timeout_ms
response_timeout_ms
first_byte_timeout_ms
idle_stream_timeout_ms
```

Custom adapters receive the route model as `model_label` by default. Built-in Gemini/ChatGPT/Qwen adapters only receive a model label when the binding has an explicit `model_labels` mapping.

## Streaming timeouts

Browser bindings may configure:

```toml
first_byte_timeout_ms = 30000
idle_stream_timeout_ms = 30000
```

The first-byte timeout applies until the first real upstream SSE event is observed. After streaming begins, the idle timeout applies between progress events. A downstream disconnect drops the lazy response body, triggering best-effort stream cancellation and ephemeral-target cleanup.

See [Browser streaming and cancellation](browser-streaming.md) for the complete v0.30 behavior.

## Diagnostics and drift

CDP adapters participate in Account Intelligence and Route Readiness.

A page that no longer matches the adapter is excluded with:

```text
browser_adapter_incompatible
```

instead of being treated as a random transport outage.

Runtime/CDP failures remain separate:

```text
browser_session_unavailable
browser_transport_error
```

This separation prevents retry storms when the provider simply changed its UI.

## Built-in adapters

For normal Gemini/ChatGPT/Qwen usage, use `browser-gemini` / `browser-chatgpt` / `browser-qwen` rather than copying their scripts into a custom adapter. See [Browser provider adapters](browser-provider-adapters.md).
