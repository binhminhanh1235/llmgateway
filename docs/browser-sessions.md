# Browser sessions

Browser sessions are first-class llmgateway accounts whose authentication state stays inside an isolated Chromium profile.

v0.27 turns the original persistence foundation into a reconciled runtime lifecycle. SQLite records metadata, but a browser-CDP route is eligible only when the live Chromium/CDP runtime is also reachable and an authenticated page matches the configured ready URL.

## Security boundary

```text
SQLite
  lifecycle status
  timestamps
  login attempt id
  last error

Isolated Chromium profile
  cookies
  Local Storage
  IndexedDB
  provider session state

llmgateway APIs
  lifecycle/readiness metadata only
  never raw browser credentials
```

On Unix, llmgateway creates the profile root and enabled session directories with mode `0700`.

Never point `browser.profile_root` at your normal Chrome profile. Each llmgateway session should own its own isolated profile directory.

CAPTCHA, 2FA, passkeys, and other authentication challenges remain interactive and must be completed normally by the user.

## Configuration

```toml
[browser]
enabled = true
profile_root = "data/browser-profiles"

[browser.sessions.gemini-web-primary]
provider = "gemini-web"
label = "Gemini web primary"
login_url = "https://gemini.google.com/app"
enabled = true

[browser.sessions.qwen-web-primary]
provider = "qwen-web"
label = "Qwen web primary"
login_url = "https://chat.qwen.ai/"
enabled = true
```

Session IDs may contain only ASCII letters, numbers, `.`, `-`, and `_`. Non-local login URLs must use HTTPS.

## v0.27 lifecycle

```text
login_required
      |
      | launch / login
      v
   starting
      |
      | authenticated page verified
      v
     ready
      |
      +---- unexpected Chromium/CDP loss ----> degraded
      |                                         |
      |                                         | auto recovery
      |                                         v
      |                                       ready
      |
      +---- auth expiry / interactive issue ---> requires_attention
      |
      +---- deliberate Stop -------------------> stopped

launch/recovery failure -----------------------> failed
```

Only `ready` is routable.

The API view also exposes `routable` so callers do not need to duplicate the lifecycle rule.

Legacy v0.12-v0.26 rows are migrated in place:

- `requires_login` becomes `login_required`;
- `login_in_progress` becomes `starting`.

Profile data is not deleted during this migration.

## Persistence and restart behavior

The `browser_session_state` table stores only lifecycle metadata:

- `session_id`
- `provider_id`
- `status`
- `login_attempt_id`
- `login_started_at`
- `last_ready_at`
- `last_verified_at`
- `last_error`
- `updated_at`

The isolated profile remains on disk across gateway and Chromium restarts.

At gateway startup, v0.27 reconciles persisted state with the actual browser runtime:

- a still-running authenticated Chromium instance is reconnected even though it is not in the new process map;
- a stale `DevToolsActivePort` is not trusted;
- a previously-ready session whose browser disappeared may be safely relaunched using the same profile;
- the relaunched browser must be re-verified before the session returns to `ready`;
- `stopped`, `login_required`, and `requires_attention` are intentional states and are not auto-launched.

Reconciliation continues periodically while llmgateway is running, so a crashed browser can recover without restarting the gateway.

## Admin API

```text
GET  /_llmgateway/browser-sessions
GET  /_llmgateway/browser-sessions/{session_id}
POST /_llmgateway/browser-sessions/{session_id}/login/start
POST /_llmgateway/browser-sessions/{session_id}/login/complete
POST /_llmgateway/browser-sessions/{session_id}/verify
POST /_llmgateway/browser-sessions/{session_id}/attention
POST /_llmgateway/browser-sessions/{session_id}/reset

POST /_llmgateway/browser-sessions/{session_id}/driver/launch
GET  /_llmgateway/browser-sessions/{session_id}/driver/status
POST /_llmgateway/browser-sessions/{session_id}/driver/verify
POST /_llmgateway/browser-sessions/{session_id}/driver/stop
```

All endpoints require the normal llmgateway API key.

The legacy `login/complete` endpoint can still mark metadata ready for non-CDP integrations such as the local `browser-http` bridge. A `browser-cdp` route additionally requires live CDP readiness at route-planning time, so a stale/manual database state cannot make a dead CDP route eligible.

## Account Readiness

Browser lifecycle is exposed through the same Account Readiness object used by Router and the UI:

- `browser_session_id`
- `browser_session_status`
- `browser_last_error`
- `browser_ready`

Unavailable browser accounts get specific reasons such as:

- `browser_session_stopped`
- `browser_login_required`
- `browser_session_degraded`
- `browser_session_requires_attention`
- `browser_session_failed`

This lets the UI explain why a browser route is unavailable without exposing secrets.
