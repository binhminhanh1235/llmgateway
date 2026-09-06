# API reference for agents

This reference reflects the llmgateway API surface at the skill's baseline. Runtime code and the installed gateway remain the source of truth.

## Authentication

Execution surface:

- `GET /v1/models`
- `POST /v1/chat/completions`
- `POST /v1/responses`
- `POST /v1/messages`

Use a scoped client credential when available.

Admin diagnostics use the global admin credential.

`GET /_llmgateway/health` is suitable as the first liveness check.

## Execution

### Model discovery

`GET /v1/models`

This is policy-filtered. Treat its result as the models the current execution credential is actually allowed to request.

### Responses

`POST /v1/responses`

Minimal body:

```json
{
  "model": "llmgateway-auto",
  "input": "Explain the task"
}
```

### Chat Completions

`POST /v1/chat/completions`

Minimal body:

```json
{
  "model": "llmgateway-auto",
  "messages": [
    {"role": "user", "content": "Explain the task"}
  ]
}
```

### Anthropic Messages

`POST /v1/messages`

Minimal body:

```json
{
  "model": "llmgateway-coding",
  "max_tokens": 1024,
  "messages": [
    {"role": "user", "content": "Explain the task"}
  ]
}
```

## Admin diagnostics

Common read endpoints:

- `GET /_llmgateway/health`
- `GET /_llmgateway/models`
- `GET /_llmgateway/accounts`
- `GET /_llmgateway/clients`
- `GET /_llmgateway/account-intelligence`
- `POST /_llmgateway/routes/explain`
- `GET /_llmgateway/model-groups`
- `GET /_llmgateway/executions`
- `GET /_llmgateway/executions/{request_id}`
- `GET /_llmgateway/accounts/{account_id}/models`
- `GET /_llmgateway/accounts/{account_id}/usage`
- `GET /_llmgateway/browser-accounts/{account_id}/runtime`
- `GET /_llmgateway/browser-sessions`
- `GET /_llmgateway/browser-sessions/{session_id}/driver/status`

### Route explain

`POST /_llmgateway/routes/explain`

Basic body:

```json
{"model": "llmgateway-auto"}
```

To let task-aware routing inspect representative input:

```json
{
  "model": "llmgateway-auto",
  "body": {
    "messages": [
      {"role": "user", "content": "Implement and debug a Rust function"}
    ]
  }
}
```

For a configured client policy, include `client_id`.

## Mutating endpoints

Mutating admin APIs exist for accounts, models, groups, browser setup/runtime and quota controls. They are deliberately not exposed as commands by the bundled helper CLI.

Read `operations.md` and inspect the current repository/API before using a mutation endpoint.
