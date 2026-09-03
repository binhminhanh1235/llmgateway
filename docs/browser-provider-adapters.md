# Browser provider adapters

llmgateway v0.15 adds the execution seam that lets a browser-backed account participate in the same router used by API accounts.

The important ownership rule remains unchanged: browser cookies, local storage, refresh state, CAPTCHA, and 2FA stay inside the isolated Chromium profile. The gateway does not export raw browser credentials through its API or provider adapter contract.

## Route lifecycle

```text
virtual model
    |
    v
Router
    |
    +-- API route ----------------------> OpenAI-compatible transport
    |
    +-- browser route
           |
           +-- account -> browser binding -> browser session
           |
           +-- session READY? -- no --> route is excluded
           |
           +-- yes
                 |
                 v
          BrowserProviderRegistry
                 |
                 v
             adapter kind
                 |
                 v
          upstream response
```

Browser routes use the normal `[[providers]]`, `[[accounts]]`, `[[routes]]`, and `[virtual_models.*]` configuration. The only extra mapping is an account-to-session binding:

```toml
[browser.bindings.gemini-web-account]
session = "gemini-web-primary"
```

The route is eligible only when that browser session is enabled and has lifecycle status `ready`.

## Adapter contract

`BrowserProviderAdapter` receives an OpenAI-shaped chat request plus the selected provider, account, route, and opaque browser session ID. It returns a normal upstream HTTP response, so the existing compatibility, streaming, quota, retry, and failover layers remain shared with API providers.

The first registered implementation is `browser-http`. It forwards the normalized chat request to an adapter bridge and adds only these metadata headers:

```text
x-llmgateway-browser-session
x-llmgateway-browser-account
x-llmgateway-route
```

It does **not** send cookies, profile directories, DevTools endpoints, credentials, or login tokens.

Example bridge provider:

```toml
[[providers]]
id = "gemini-web-bridge"
kind = "browser-http"
base_url = "http://127.0.0.1:7441/v1"
models_path = "models"

[[accounts]]
id = "gemini-web-account"
provider = "gemini-web-bridge"
# The generic v0.15 account schema still requires this field. browser-http does not read it.
api_key_env = "BROWSER_ACCOUNT_UNUSED"
auth_style = "bearer"
enabled = true
discover_models = false

[browser.bindings.gemini-web-account]
session = "gemini-web-primary"

[[routes]]
id = "gemini-web-route"
account = "gemini-web-account"
model = "gemini-web-model"
priority = 10
enabled = true
capabilities = ["chat"]
```

`browser-http` is deliberately a narrow contract/test bridge, not a claim that Gemini or Qwen expose a supported private web API. Provider-specific transports belong behind new adapter kinds such as `browser-gemini` or `browser-qwen` and must respect each service's terms and normal authentication challenges.

## Authentication expiry

A `401` or `403` from a browser adapter has browser-specific meaning. llmgateway:

1. records the route/account failure through the normal quota and health path,
2. transitions the bound browser session to `requires_attention`,
3. continues failover to the next healthy route,
4. excludes that browser route from future plans until the user signs in again and the session returns to `ready`.

This makes an expired browser login visible in the Accounts UI instead of creating an invisible retry loop.

## Why the router is shared

There is intentionally no second "browser router". Browser-backed models are another execution lane behind the same route planner. That preserves one place for priority, health, quota pressure, cooldown, affinity, and future task-aware scoring.

## Next adapter milestone

The next step is a provider-specific adapter that can use the already-running isolated Chromium profile without exporting session secrets. Gemini/Qwen implementations should remain separate modules so web UI changes cannot destabilize the gateway core.
