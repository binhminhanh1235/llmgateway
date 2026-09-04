# Browser provider adapters

llmgateway treats browser accounts as first-class execution providers behind the same Router used by API accounts.

The security and ownership boundary is deliberate:

- cookies, local storage, refresh state and provider authentication stay inside the isolated Chromium profile;
- CAPTCHA, 2FA, passkeys and anti-abuse challenges remain normal interactive provider flows;
- llmgateway never exports raw browser credentials through its APIs;
- llmgateway owns conversation/context state; provider-native chats are execution surfaces for individual turns.

## v0.28 provider kinds

v0.28 adds two built-in browser provider kinds:

```text
browser-gemini
browser-chatgpt
browser-qwen
```

The generic integration lanes remain available:

```text
browser-cdp    trusted local contract-v1 adapter script
browser-http   local bridge/test transport
```

Built-in Gemini/ChatGPT/Qwen adapters are embedded into the llmgateway binary. They do not require an `adapter_script` path.

## Example: Gemini Web

```toml
[browser.sessions.gemini-web-primary]
provider = "gemini-web"
label = "Gemini web primary"
login_url = "https://gemini.google.com/app"
enabled = true

[browser.bindings.gemini-web-account]
session = "gemini-web-primary"
adapter_contract_version = 1
models = ["gemini-web-pro"]
# Optional. If omitted, keep the model already selected in Gemini UI.
model_labels = { "gemini-web-pro" = "Pro" }
ephemeral_chat = true

[chromium.sessions.gemini-web-primary]
enabled = true
ready_url_prefixes = ["https://gemini.google.com/app"]

[[providers]]
id = "gemini-web"
kind = "browser-gemini"

[[accounts]]
id = "gemini-web-account"
provider = "gemini-web"
enabled = true

[[routes]]
id = "gemini-web-pro"
account = "gemini-web-account"
model = "gemini-web-pro"
priority = 5
enabled = true
capabilities = ["chat", "reasoning", "long-context"]
```

## Example: ChatGPT Web

```toml
[browser.sessions.chatgpt-web-primary]
provider = "chatgpt-web"
label = "ChatGPT web primary"
login_url = "https://chatgpt.com/"
enabled = true

[browser.bindings.chatgpt-web-account]
session = "chatgpt-web-primary"
adapter_contract_version = 1
models = ["chatgpt-web-default"]
# Optional. If omitted, keep the model already selected in ChatGPT UI.
# model_labels = { "chatgpt-web-default" = "Thinking" }
ephemeral_chat = true

[chromium.sessions.chatgpt-web-primary]
enabled = true
ready_url_prefixes = ["https://chatgpt.com/"]

[[providers]]
id = "chatgpt-web"
kind = "browser-chatgpt"

[[accounts]]
id = "chatgpt-web-account"
provider = "chatgpt-web"
enabled = true

[[routes]]
id = "chatgpt-web-default"
account = "chatgpt-web-account"
model = "chatgpt-web-default"
priority = 5
enabled = true
capabilities = ["chat", "coding", "reasoning"]
```

The adapter does not force a ChatGPT UI model unless `model_labels` maps the logical route model to a visible provider label. This keeps the integration resilient to account-, plan-, and UI-specific model names.

## Example: Qwen Web

```toml
[browser.sessions.qwen-web-primary]
provider = "qwen-web"
label = "Qwen web primary"
login_url = "https://chat.qwen.ai/"
enabled = true

[browser.bindings.qwen-web-account]
session = "qwen-web-primary"
adapter_contract_version = 1
models = ["qwen-web-coder"]
model_labels = { "qwen-web-coder" = "Qwen3-Coder" }
ephemeral_chat = true

[chromium.sessions.qwen-web-primary]
enabled = true
ready_url_prefixes = ["https://chat.qwen.ai/"]

[[providers]]
id = "qwen-web"
kind = "browser-qwen"

[[accounts]]
id = "qwen-web-account"
provider = "qwen-web"
enabled = true

[[routes]]
id = "qwen-web-coder"
account = "qwen-web-account"
model = "qwen-web-coder"
priority = 5
enabled = true
capabilities = ["chat", "coding", "reasoning"]
```

Browser accounts need no dummy API key and API model discovery is disabled automatically.

## Adapter contract v1

All CDP adapters now use a versioned page-context contract.

An adapter exposes:

```js
globalThis.__LLMGATEWAY_ADAPTER__ = {
  meta: {
    contract_version: 1,
    id: "provider-adapter-id",
    provider: "provider-name",
    adapter_version: "..."
  },

  async probe(context) {
    return {
      ok: true,
      code: "ready",
      message: "compatible page detected",
      page_signature: "provider-page-v1"
    };
  },

  async chat(request, context) {
    return {
      status: 200,
      content_type: "application/json",
      body: { /* OpenAI Chat Completions-shaped response */ }
    };
  }
};
```

The gateway validates the contract and adapter identity before trusting the result.

The probe runs before a CDP-backed route becomes eligible. This gives provider UI drift a dedicated failure state instead of pretending it is a generic network error.

## Adapter diagnostics

Account Intelligence exposes adapter-level state independently from Chromium/session state:

```text
browser_adapter.status
browser_adapter.adapter_id
browser_adapter.adapter_version
browser_adapter.contract_version
browser_adapter.expected_contract_version
browser_adapter.message
browser_adapter.page_signature
browser_adapter.configured_models
```

Readiness reasons include:

```text
browser_adapter_incompatible
browser_adapter_login_required
browser_adapter_unavailable
```

Execution Trace can distinguish:

```text
browser_adapter_incompatible
browser_model_unavailable
browser_session_unavailable
browser_transport_error
```

This matters because the remedies are different:

- browser process/CDP unavailable: runtime recovery can help;
- login required: user must authenticate normally;
- adapter incompatible: provider UI/selectors changed, so retrying the same route blindly is pointless;
- model unavailable: fix model mapping/provider UI choice.

## Page-drift recovery

Built-in adapters carry several selector fallbacks, but provider web UIs can change without notice.

A binding can temporarily override selector groups without rebuilding llmgateway:

```toml
[browser.bindings.gemini-web-account]
session = "gemini-web-primary"
selector_overrides = { input = ["div[aria-label='Enter a prompt for Gemini']"], send = ["button[aria-label='Send message']"] }
```


Selector overrides are a recovery mechanism, not a way to bypass authentication or provider controls.

## Model selection

The route `model` is the stable llmgateway model ID.

Provider web UIs often display a different label, so bindings can map logical IDs to UI labels:

```toml
model_labels = {
  "gemini-web-pro" = "Pro",
  "gemini-web-flash" = "Flash"
}
```

Built-in adapters only touch the provider model picker when a mapping exists. Without a mapping they leave the currently selected provider UI model unchanged. This avoids guessing unstable provider display names.

The optional `models` list is an account binding allowlist. A route requesting a model outside that list fails as `browser_model_unavailable`.

## Stateless provider tabs

Built-in Gemini/ChatGPT/Qwen adapters default to:

```toml
ephemeral_chat = true
```

For each gateway request:

```text
existing authenticated Chromium profile
             |
             v
create fresh provider chat tab
             |
             v
send llmgateway-compiled context
             |
             v
read result
             |
             v
close provider tab
```

Cookies/authentication are reused from the profile, while provider-native conversation history is not silently reused.

This preserves llmgateway's core ownership rule:

> The conversation belongs to llmgateway, not to a provider-native chat ID.

Set `ephemeral_chat = false` only when intentionally using a persistent provider page.

Persistent llmgateway threads are a separate concept. Gemini and ChatGPT thread requests use provider-native conversation affinity: the first turn creates and records the native provider conversation, later turns reuse the same open tab when possible, and a missing tab is reopened once from the persisted native URL. ChatGPT conversations are recognized by their `/c/<conversation-id>` URL identity. Only provider-missed/current turns are sent. See [Provider-native conversation affinity](provider-conversation-affinity.md).

## Coding-agent tool bridge

Claude Code, Codex and OpenCode depend on tool/function calls. Provider web UIs do not expose the same native OpenAI/Anthropic tool API, so built-in adapters implement a prompt-mediated compatibility bridge.

When the normalized request contains `tools`, the adapter:

1. serializes function names, descriptions and JSON schemas into a tool protocol;
2. sends prior assistant tool calls and tool results as part of the compiled turn history;
3. asks the web model to emit a strict llmgateway tool envelope when a client tool is required;
4. parses that envelope;
5. converts it into OpenAI-compatible `tool_calls`;
6. lets the existing Anthropic/Responses compatibility layer translate it for Claude Code or Codex.

Both non-streaming and v0.30 incremental browser streaming emit standard OpenAI tool-call shapes. Tool-call envelopes remain buffered until the adapter can validate and normalize the complete tool-call payload, while ordinary text is emitted incrementally.

This is a compatibility bridge, not provider-native function calling. Model compliance with the prompt protocol is therefore best-effort. v0.30 adds incremental text streaming and cancellation without changing that tool-bridge limitation.

## Failure semantics

The normal Router still owns fallback.

Examples:

```text
Gemini A: adapter incompatible
        -> excluded

Gemini B: ready
        -> selected

Gemini B: login expires
        -> requires_attention

Qwen A: ready
        -> selected

all browser accounts unavailable
        -> optional API fallback
```

HTTP 401/403 from a browser provider marks its bound session `requires_attention`. Runtime transport failures can move a CDP session to `degraded`, allowing the v0.27 reconciler to recover it safely.

Adapter incompatibility does not intentionally destroy or reset the browser profile. The account stays diagnosable so selectors/adapter code can be updated without forcing a fresh login.

## Deterministic CI

v0.28 does not depend on live Gemini/Qwen websites in CI.

The suite includes:

- Node fake-page fixtures for Gemini/ChatGPT/Qwen probe behavior;
- healthy composer detection;
- login-required detection;
- page-drift / missing-selector detection;
- coding-agent tool-call conversion;
- fake CDP target/Runtime.evaluate fixtures;
- contract metadata/version validation;
- adapter health/page-signature diagnostics.

Live provider pages can still evolve between releases, which is why diagnostics and selector overrides are first-class product behavior rather than hidden implementation details.

## Security and service constraints

Browser adapters must respect provider terms, quota limits, anti-abuse controls and normal authentication flows.

llmgateway does not:

- solve or bypass CAPTCHA;
- automate 2FA/passkey challenges;
- export raw cookies;
- bypass provider rate limits;
- make an unavailable model appear available;
- treat a changed provider UI as healthy.

The browser integration is designed to fail visibly and fall back safely.
