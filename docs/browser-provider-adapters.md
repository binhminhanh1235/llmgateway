# Browser provider adapters

llmgateway lets browser-backed accounts participate in the same router used by API accounts.

The ownership rule is simple: browser cookies, local storage, refresh state, CAPTCHA, and 2FA stay inside the isolated Chromium profile. The gateway does not export raw browser credentials through its API or provider adapter contract.

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

## First-class browser accounts (v0.17)

Browser accounts no longer need fake API credentials or an explicit model-discovery switch. The provider kind defines the transport boundary.

```toml
[[providers]]
id = "gemini-web"
kind = "browser-cdp"

[[accounts]]
id = "gemini-web-account"
provider = "gemini-web"
enabled = true
```

For `browser-*` providers, llmgateway automatically:

- treats the account transport as browser-backed;
- does not require `api_key_env`;
- disables API model discovery;
- prevents the account from being used as a hybrid-retrieval embedding backend.

API accounts are unchanged and still default to model discovery enabled:

```toml
[[accounts]]
id = "openrouter-main"
provider = "openrouter"
api_key_env = "OPENROUTER_API_KEY"
enabled = true
```

## Adapter contract

`BrowserProviderAdapter` receives an OpenAI-shaped chat request plus the selected provider, account, route, and opaque browser session ID. It returns a normal upstream HTTP response, so compatibility, quota, retry, and failover behavior stay shared with API providers.

Two execution lanes are available:

- `browser-http`: a narrow local bridge/test contract that forwards opaque session/account/route IDs but never cookies or profile secrets.
- `browser-cdp`: executes a trusted local provider adapter inside an already-authenticated Chromium page over loopback CDP.

Example CDP provider:

```toml
[[providers]]
id = "gemini-web"
kind = "browser-cdp"

[[accounts]]
id = "gemini-web-account"
provider = "gemini-web"
enabled = true

[browser.bindings.gemini-web-account]
session = "gemini-web-primary"
target_url_prefix = "https://gemini.google.com/app"
adapter_script = "adapters/gemini.js"

[[routes]]
id = "gemini-web-route"
account = "gemini-web-account"
model = "gemini-web-model"
priority = 10
enabled = true
capabilities = ["chat"]
```

Provider-specific adapter scripts remain experimental integration code. They must respect service terms and normal authentication, anti-abuse, and quota controls. llmgateway does not automate CAPTCHA/2FA or expose raw browser session secrets.

## Authentication expiry

A `401` or `403` from a browser adapter has browser-specific meaning. llmgateway:

1. records the route/account failure through the normal quota and health path,
2. transitions the bound browser session to `requires_attention`,
3. continues failover to the next healthy route,
4. excludes that browser route from future plans until the user signs in again and the session returns to `ready`.

This makes an expired browser login visible in the Accounts UI instead of creating an invisible retry loop.

## Why the router is shared

There is intentionally no second browser router. Browser-backed models are another execution lane behind the same route planner. That preserves one place for priority, health, quota pressure, cooldown, affinity, and future task-aware scoring.
