# Operations and safety

## READ

Safe inspection:

- health;
- public model discovery;
- admin model/account/group/client listings;
- route explain;
- usage/intelligence;
- execution trace;
- browser runtime/status.

READ must not mutate gateway state.

## EXECUTE

Inference through:

- Responses;
- Chat Completions;
- Anthropic Messages;
- persistent Threads when requested.

Execution consumes quota/budget, so respect the caller's client policy and requested limits.

## OPERATE

Examples:

- enable/disable an account;
- enable/disable an account model or catalog model;
- enable/disable a model group;
- refresh provider model discovery;
- launch/verify/stop/restart browser runtime;
- change browserless transport preference;
- reset quota counters.

Only operate when the user clearly asked for the state change or the approved task necessarily includes it.

Before mutation:

1. identify the exact resource;
2. state the intended change;
3. preserve reversible configuration where possible;
4. re-read current state to avoid acting on stale assumptions;
5. verify post-state.

## ADMIN / destructive

Examples:

- delete an account;
- remove model groups;
- change credentials/secrets;
- rewrite broad client authorization policy;
- destructive config/data changes.

Require explicit approval for the specific destructive action.

A generic "fix it", "make it work", "diagnose it", or "continue" is not approval to delete resources or expose secrets.

## Secret handling

Never return or commit:

- API keys;
- browser cookies;
- local storage/session tokens;
- refresh tokens;
- profile secrets;
- raw authentication snapshots.

Use environment-variable names in commands and documentation, never secret values.

## Provider authentication

Interactive authentication remains user-controlled.

Agents may open/start a login flow when asked, but must not attempt to defeat CAPTCHA, 2FA, passkeys, anti-abuse controls or provider quota enforcement.
