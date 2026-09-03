# Browser session foundation

llmgateway v0.12 adds the lifecycle and storage boundary needed for future Gemini/Qwen web adapters without putting browser cookies into the gateway database or API.

## Security boundary

Browser session secrets belong to the browser profile, not to llmgateway application state.

```text
SQLite
  status
  timestamps
  login attempt id
  last error

Isolated Chromium profile
  cookies
  Local Storage
  IndexedDB
  provider session state

llmgateway API
  lifecycle metadata only
  never raw cookies
```

On Unix, llmgateway creates the profile root and enabled session directories with mode `0700`.

The default profile root is:

```text
data/browser-profiles
```

Browser sessions are opt-in. If `[browser]` is absent, the registry is disabled and the profile directory is not created.

## Configuration

```toml
[browser]
enabled = true
profile_root = "data/browser-profiles"

[browser.sessions.gemini-web-primary]
provider = "gemini"
label = "Gemini web primary"
login_url = "https://gemini.google.com/app"
enabled = true

[browser.sessions.qwen-web-primary]
provider = "qwen"
label = "Qwen web primary"
login_url = "https://chat.qwen.ai/"
enabled = true
```

Session IDs may contain only ASCII letters, numbers, `.`, `-`, and `_`. This prevents a configured session ID from escaping the profile root through path traversal.

Non-local login URLs must use HTTPS.

## Lifecycle

```text
requires_login
      |
      | POST .../login/start
      v
login_in_progress
      |
      | normal browser login completed
      v
    ready
      |
      | adapter detects expiry / challenge / invalid session
      v
requires_attention
      |
      | POST .../reset
      v
requires_login
```

`login/start` prepares the isolated profile directory and creates a login-attempt ID. v0.12 deliberately does not launch or control Chromium yet.

`login/complete` marks the lifecycle state ready. It does not prove that provider authentication is valid by itself. A future browser driver will call it only after validating the authenticated page/session.

CAPTCHA, 2FA, passkeys, and other login challenges must be completed normally by the user. Browser adapters must never attempt to bypass them.

## Admin API

```text
GET  /_llmgateway/browser-sessions
GET  /_llmgateway/browser-sessions/{session_id}
POST /_llmgateway/browser-sessions/{session_id}/login/start
POST /_llmgateway/browser-sessions/{session_id}/login/complete
POST /_llmgateway/browser-sessions/{session_id}/verify
POST /_llmgateway/browser-sessions/{session_id}/attention
POST /_llmgateway/browser-sessions/{session_id}/reset
```

All endpoints require the normal llmgateway API key.

Example start response:

```json
{
  "login_attempt_id": "browser_login_...",
  "login_url": "https://gemini.google.com/app",
  "profile_dir": "data/browser-profiles/gemini-web-primary",
  "session": {
    "id": "gemini-web-primary",
    "status": "login_in_progress"
  },
  "instructions": ["..."]
}
```

The response contains paths and lifecycle metadata, never cookie values.

## Persistence

SQLite table `browser_session_state` stores:

- `session_id`
- `provider_id`
- `status`
- `login_attempt_id`
- `login_started_at`
- `last_ready_at`
- `last_verified_at`
- `last_error`
- `updated_at`

The profile remains on disk across gateway restarts, so the future browser driver can reopen the same authenticated browser state.

Resetting lifecycle state does not delete the profile directory. This is intentional: reset means “require login/verification again”, not “destroy local browser data”. A destructive profile wipe should be a separate explicit operation in a later release.

## What v0.12 does not do

v0.12 does not reverse-engineer Gemini/Qwen private web endpoints and does not use browser sessions for chat routing yet.

The next adapter layer can build on this foundation with:

1. a Chromium process/driver using the isolated profile,
2. authenticated-session verification,
3. provider-specific chat transport behind the existing gateway provider abstraction,
4. `requires_attention` transitions on session expiry or interactive challenges,
5. route eligibility tied to browser-session readiness.

Keeping those concerns separate makes the fragile web-integration layer replaceable while the conversation, routing, quota, and lifecycle layers remain stable.
