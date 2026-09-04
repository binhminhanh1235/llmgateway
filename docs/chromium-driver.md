# Chromium driver

The Chromium driver manages isolated browser profiles for browser-backed llmgateway accounts. v0.27 adds restart reconciliation, stale-CDP detection, automatic recovery, and deliberate-stop semantics.

The driver does not read or export cookies, submit credentials, solve CAPTCHA, or automate 2FA. Authentication remains a normal user interaction inside Chromium.

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

[chromium]
enabled = true
# executable = "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"
startup_timeout_seconds = 15
auto_recover = true
reconcile_interval_seconds = 15
extra_args = []

[chromium.sessions.gemini-web-primary]
enabled = true
ready_url_prefixes = ["https://gemini.google.com/app"]
```

`reconcile_interval_seconds` must be between 5 and 3600 seconds.

`ready_url_prefixes` are not credentials. They are sanitized page locations used as evidence that the interactive login reached the authenticated application.

## Runtime status

`GET .../driver/status` exposes:

- `running`: either a process managed by this gateway instance or a reachable Chromium CDP runtime;
- `managed`: whether the current gateway instance launched and owns the process handle;
- `pid`: available for a managed process;
- `debugger_port`;
- `debugger_reachable`;
- sanitized `pages`;
- `ready_match`.

Query strings, fragments, usernames, and passwords are stripped from page URLs before they are returned.

A gateway restart can therefore produce:

```json
{
  "running": true,
  "managed": false,
  "debugger_reachable": true,
  "ready_match": "https://gemini.google.com/app"
}
```

This is expected: Chromium survived the gateway restart and v0.27 reconnected to its loopback CDP endpoint.

## Startup reconciliation

Before the HTTP listener starts, llmgateway reconciles every enabled Chromium session.

The important cases are:

| Persisted/runtime state | v0.27 behavior |
| --- | --- |
| ready + live authenticated Chromium | mark/keep ready |
| ready + Chromium survived gateway restart | reconnect and keep ready |
| ready/degraded + Chromium crashed | remove stale CDP metadata, relaunch same profile, verify |
| browser alive but authenticated page missing | login_required/degraded; not routable |
| stale DevToolsActivePort | never treated as a live browser |
| stopped | do not auto-launch |
| requires_attention | do not auto-launch |
| login_required | do not auto-launch |
| recovery launch/verify fails | failed with diagnostic error |

The same reconciliation runs periodically using `reconcile_interval_seconds`.

## Automatic recovery

With `auto_recover = true`, an unexpectedly lost session that was previously active can be relaunched using its existing isolated profile.

Recovery is deliberately conservative:

1. verify the old CDP runtime is actually gone;
2. remove a stale `DevToolsActivePort` only when it is unreachable;
3. launch Chromium with the same `--user-data-dir`;
4. wait for CDP;
5. verify an authenticated page;
6. only then restore lifecycle `ready`.

If provider authentication has expired, the browser remains available for normal interactive login but the route does not become ready automatically.

## Deliberate stop

`POST .../driver/stop` marks the session `stopped`.

A stopped session is not auto-recovered. The profile remains on disk so a later Launch can reuse it.

When Chromium survived a gateway restart, the new gateway has no OS child-process handle for it. v0.27 closes that reconnected browser through loopback CDP `Browser.close` instead of merely deleting `DevToolsActivePort`, avoiding an orphaned process/profile lock.

## Live route safety

For `browser-cdp` providers, Router readiness checks the live Chromium runtime at plan time. A route requires:

- session lifecycle `ready`;
- Chromium/CDP reachable;
- a configured ready URL currently visible.

This closes the gap between background reconciliation ticks: if Chromium dies immediately before a request, the dead browser route is skipped and failover can continue.

## API endpoints

```text
POST /_llmgateway/browser-sessions/{session_id}/driver/launch
GET  /_llmgateway/browser-sessions/{session_id}/driver/status
POST /_llmgateway/browser-sessions/{session_id}/driver/verify
POST /_llmgateway/browser-sessions/{session_id}/driver/stop
```

See [Browser sessions](browser-sessions.md) for lifecycle and persistence semantics, and [Browser provider adapters](browser-provider-adapters.md) for the execution boundary.
