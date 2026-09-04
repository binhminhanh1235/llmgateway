# Browser Accounts UX

v0.29 makes Gemini Web and Qwen Web browser accounts manageable from the local llmgateway UI without hand-authoring the linked TOML sections.

## Managed setup flow

Open **Accounts** and choose **Add browser account**.

1. Choose **Gemini Web** or **Qwen Web**.
2. Optionally set an account ID, display label, logical model ID/model label, and route priority.
3. llmgateway validates the full generated configuration before replacing the active config file.
4. The linked browser session, provider binding, Chromium session, account, route, and virtual-model membership are hot-activated.
5. Choose **Login with browser**.
6. Complete the provider login, CAPTCHA, and 2FA normally in the isolated Chromium window.
7. llmgateway verifies the authenticated page and probes the provider adapter.
8. The route becomes eligible immediately when session, CDP runtime, and adapter diagnostics are ready.

A gateway process restart is not required after managed account creation.

## What the wizard creates

For a managed account, llmgateway keeps these pieces aligned as one setup unit:

- `[browser.sessions.<id>]`
- `[browser.bindings.<account>]`
- `[chromium.sessions.<id>]`
- matching `[[providers]]` entry when needed
- `[[accounts]]`
- `[[routes]]`
- route membership in the relevant virtual models

Generated config is parsed and validated before it replaces the previous file. The writer uses a temporary file plus backup, and duplicate account IDs are rejected.

## Hot activation model

Hot reload follows an immutable-snapshot rule:

```text
request N  -> config snapshot A
wizard     -> validate + prepare config B
runtime    -> reload browser/session/provider/catalog state
swap       -> LiveConfig B
request N+1-> config snapshot B
```

Routing and execution for one request are pinned to the same `Arc<AppConfig>` snapshot. A request therefore never plans a route from one config version and executes it against another.

## Lifecycle controls

Managed browser account cards expose:

- **Login with browser** / **Open session**
- **Verify session**
- **Disable account**
- **Enable account**
- **Re-authenticate**
- **Restart browser**
- **Stop browser**
- **Reset** when recovery requires it

Disabling an account removes it from routing without deleting its Chromium profile. Stopping Chromium also preserves the profile. Re-enabling can reuse an already authenticated profile/session when it is still valid.

Re-authentication intentionally resets the session lifecycle and opens the same isolated profile again so the user can complete provider authentication normally.

## Readiness and diagnostics

A browser route is eligible only when all required layers agree:

```text
account enabled
  + configured route
  + browser session ready
  + Chromium/CDP reachable
  + authenticated provider page matched
  + provider adapter probe ready
  + model allowed
  + normal quota/health/task routing checks
```

The Accounts UI surfaces lifecycle and adapter states such as login required, recovering, recovery failed, and adapter/page incompatibility. Provider-page drift remains an explicit adapter diagnostic rather than a generic network failure.

## Admin endpoints

All endpoints below use the normal llmgateway API-key authentication unless noted.

```text
GET   /_llmgateway/browser-account-setup/providers
POST  /_llmgateway/browser-account-setup
PATCH /_llmgateway/browser-account-setup/{account_id}

GET   /_llmgateway/browser-sessions
GET   /_llmgateway/browser-sessions/{session_id}
POST  /_llmgateway/browser-sessions/{session_id}/driver/launch
GET   /_llmgateway/browser-sessions/{session_id}/driver/status
POST  /_llmgateway/browser-sessions/{session_id}/driver/verify
POST  /_llmgateway/browser-sessions/{session_id}/driver/stop
POST  /_llmgateway/browser-sessions/{session_id}/reset
```

Create example:

```bash
curl -X POST http://127.0.0.1:7331/_llmgateway/browser-account-setup \
  -H "Authorization: Bearer $LLMGATEWAY_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "provider": "qwen",
    "account_id": "qwen-personal",
    "label": "Qwen Personal",
    "priority": 10
  }'
```

A successful v0.29 create response includes `"restart_required": false`.

Disable or re-enable:

```bash
curl -X PATCH http://127.0.0.1:7331/_llmgateway/browser-account-setup/qwen-personal \
  -H "Authorization: Bearer $LLMGATEWAY_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{"enabled": false}'
```

## Security boundary

v0.29 does not change the browser-account security boundary:

- cookies and local-storage authentication remain inside the isolated Chromium profile;
- raw browser credentials are not written into llmgateway config or returned by the APIs;
- DevTools remains loopback-local;
- CAPTCHA and 2FA stay interactive;
- stopping or disabling an account does not export or copy browser secrets.

## CI contract

The v0.29 release gate includes a deterministic browser-account UX smoke test. It starts with no browser account, creates a managed Qwen account while the gateway is running, validates hot catalog/session visibility, proves the route is ineligible before login, launches a fake CDP browser, verifies an authenticated page and adapter contract, executes through the browser route, tests API fallback after disable, re-enables the account without restart, and verifies duplicate-create conflict handling.

The fake CDP fixture avoids depending on live Gemini/Qwen websites while still exercising the real Chromium/CDP and adapter-contract path.
