# Chromium login driver

llmgateway v0.13 can launch a local Chrome/Chromium process for an opt-in browser session while keeping the provider session inside an isolated browser profile.

The driver is deliberately narrow. It does not read cookies, copy cookies into SQLite, submit credentials, solve CAPTCHA, or automate 2FA. The user completes normal interactive authentication in the browser. The driver only manages the browser process and observes sanitized page URLs through the loopback DevTools endpoint so it can decide whether login reached a configured authenticated page.

## Security boundary

```text
llmgateway SQLite
  lifecycle/status/timestamps only

isolated profile directory (0700 on Unix)
  cookies
  Local Storage
  IndexedDB
  provider session state

Chromium DevTools on 127.0.0.1
  page URL observation only
  query strings and fragments removed before API responses
```

Never configure `browser.profile_root` to point at your normal Chrome profile. llmgateway creates and owns dedicated profile directories for these sessions.

## Configuration

Browser-session persistence and Chromium launching are separate capabilities. Both must be enabled for automatic launch/verification.

```toml
[browser]
enabled = true
profile_root = "data/browser-profiles"

[browser.sessions.gemini-web-primary]
provider = "gemini"
label = "Gemini web primary"
login_url = "https://gemini.google.com/app"
enabled = true

[chromium]
enabled = true
# Optional. If omitted, llmgateway searches common Chrome/Chromium executable names.
# executable = "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"
startup_timeout_seconds = 15
extra_args = []

[chromium.sessions.gemini-web-primary]
enabled = true
ready_url_prefixes = ["https://gemini.google.com/app"]
```

Qwen example:

```toml
[browser.sessions.qwen-web-primary]
provider = "qwen"
label = "Qwen web primary"
login_url = "https://chat.qwen.ai/"
enabled = true

[chromium.sessions.qwen-web-primary]
enabled = true
ready_url_prefixes = ["https://chat.qwen.ai/"]
```

`ready_url_prefixes` are not authentication credentials. They are simply the page locations that llmgateway treats as evidence that the normal interactive login flow reached the authenticated application.

## API flow

All endpoints require the normal llmgateway API key.

Launch Chromium and begin a login attempt:

```bash
curl -X POST \
  http://127.0.0.1:7331/_llmgateway/browser-sessions/gemini-web-primary/driver/launch \
  -H "Authorization: Bearer $LLMGATEWAY_API_KEY"
```

The process receives a dedicated `--user-data-dir`, loopback-only remote debugging, and the configured login URL. Complete login normally in the opened browser window.

Inspect process state and sanitized page locations:

```bash
curl \
  http://127.0.0.1:7331/_llmgateway/browser-sessions/gemini-web-primary/driver/status \
  -H "Authorization: Bearer $LLMGATEWAY_API_KEY"
```

Ask llmgateway to verify the current page against `ready_url_prefixes`:

```bash
curl -X POST \
  http://127.0.0.1:7331/_llmgateway/browser-sessions/gemini-web-primary/driver/verify \
  -H "Authorization: Bearer $LLMGATEWAY_API_KEY"
```

When a ready URL matches, the browser-session lifecycle automatically moves to `ready` and records `last_ready_at` / `last_verified_at`.

Stop the Chromium process managed by llmgateway:

```bash
curl -X POST \
  http://127.0.0.1:7331/_llmgateway/browser-sessions/gemini-web-primary/driver/stop \
  -H "Authorization: Bearer $LLMGATEWAY_API_KEY"
```

Stopping the browser does not delete the profile. A later launch reuses the same isolated profile so a still-valid provider session can survive process restarts.

## API endpoints

```text
POST /_llmgateway/browser-sessions/{session_id}/driver/launch
GET  /_llmgateway/browser-sessions/{session_id}/driver/status
POST /_llmgateway/browser-sessions/{session_id}/driver/verify
POST /_llmgateway/browser-sessions/{session_id}/driver/stop
```

The older browser-session lifecycle APIs remain available and are intentionally independent from the Chromium launcher.

## Failure behavior

- Missing Chrome/Chromium: launch returns a configuration error; the gateway continues serving API providers normally.
- Browser launch failure: the browser session is moved to `requires_attention` with a diagnostic error.
- DevTools startup timeout: the managed process is stopped and the session is moved to `requires_attention`.
- Browser exits before verification: `driver/verify` moves an in-progress login to `requires_attention`.
- Login challenge/CAPTCHA/2FA: the driver does nothing special; complete it normally in the browser.

## What comes next

The Chromium driver is process/session infrastructure, not a Gemini or Qwen web-chat adapter. The next layer can use a `ready` browser session to implement provider-specific web operations while keeping browser secrets inside the profile and preserving llmgateway's canonical thread/context model.
