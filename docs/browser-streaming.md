# Browser streaming and cancellation

v0.30 makes built-in Gemini Web and Qwen Web routes truly incremental. The gateway no longer waits for the provider page to finish the entire DOM response before returning browser-backed streaming data.

## Data path

```text
Gemini / Qwen DOM changes
        |
        v
built-in adapter stream state
  streamStart / streamPoll / streamCancel
        |
        v
loopback CDP Runtime.evaluate
        |
        v
lazy reqwest response body
        |
        +--> OpenAI Chat SSE
        +--> OpenAI Responses event translation
        +--> Anthropic Messages event translation
        +--> persistent-thread capture
        |
        v
client
```

The page keeps cumulative answer text plus the amount already delivered. It does not append every DOM observation to an unbounded queue. Each poll returns only the new suffix.

## Backpressure

CDP polling happens inside the lazy response body.

When the downstream client reads slowly, the gateway polls slowly. The browser adapter retains the latest cumulative answer and the delivered offset, so it can produce the missing delta on the next poll without building an unbounded gateway-side chunk queue.

This is deliberate pull-based backpressure rather than a second producer queue.

## Cancellation

A browser stream owns a cleanup guard.

If the downstream body is dropped before a normal terminal event, for example because the client disconnects:

1. llmgateway drops the upstream stream generator;
2. the CDP stream cleanup guard invokes `streamCancel({stream_id})`;
3. the built-in adapter marks the page-side producer cancelled and clicks the provider Stop control when available;
4. an ephemeral provider tab is closed;
5. Execution Trace records a cancelled stream and whether any partial response was already emitted.

Cleanup is best-effort if Chromium itself is already gone, but no normal disconnect path intentionally leaves an in-flight provider generation running.

## Terminal completion

OpenAI-compatible browser streams end with:

```text
data: <chat.completion.chunk>

...
data: [DONE]
```

The tracing layer treats the `[DONE]` marker itself as terminal. It does not wait for a later transport EOF, because a client may close immediately after receiving the terminal SSE frame.

## Timeouts

Managed browser accounts default to:

```toml
first_byte_timeout_ms = 30000
idle_stream_timeout_ms = 30000
```

They live under the corresponding `[browser.bindings.<account>]` table.

`first_byte_timeout_ms` applies until the first real browser stream event is emitted.

`idle_stream_timeout_ms` applies after streaming has started and measures the maximum interval without progress.

Both values must be between 500 and 120000 milliseconds.

The older `response_timeout_ms` still bounds the provider-page generation itself. The stream timeouts are gateway-side delivery guarantees layered on top of that provider response bound.

## Provider text rewrites

Gemini/Qwen pages expose cumulative rendered text. Normally each new DOM observation extends the prior text.

If the provider rewrites text that has already been emitted instead of extending it, the built-in stream detects that the new cumulative text no longer starts with the delivered prefix and fails with:

```text
stream_rewrite_detected
```

This is safer than silently duplicating or mutating text that a client already consumed.

## Tool calls

The browser tool bridge is prompt-mediated rather than provider-native function calling.

Ordinary assistant text streams incrementally. A possible `[[LLMGATEWAY_TOOL_CALLS]]` envelope is held until the response is complete so the adapter can validate and normalize it before emitting the OpenAI-compatible `tool_calls` delta and terminal `finish_reason = "tool_calls"`.

This avoids leaking a partial internal tool envelope to coding-agent clients.

## Protocol compatibility

The raw browser stream is OpenAI Chat Completions-shaped SSE. Translation stays above the browser provider layer:

- `POST /v1/chat/completions` forwards the stream;
- `POST /v1/responses` converts each upstream chat delta into Responses events;
- `POST /v1/messages` converts each upstream chat delta into Anthropic Messages events;
- persistent threads capture the same stream while preserving llmgateway-owned conversation state.

The raw stream is traced before protocol translation, so first-byte and byte/chunk metrics describe the actual provider/gateway path rather than encoder overhead.

## Execution Trace

A streaming execution can include:

```json
{
  "status": "success",
  "stream": {
    "first_byte_ms": 24,
    "chunk_count": 5,
    "byte_count": 712,
    "outcome": "completed",
    "partial_response": false,
    "error": null
  }
}
```

A client disconnect after receiving content can instead end as:

```json
{
  "status": "cancelled",
  "stream": {
    "outcome": "cancelled",
    "partial_response": true,
    "error": "downstream stream dropped before completion"
  }
}
```

Trace Console surfaces the same fields.

## Browser-to-API fallback

Fallback is still decided before the gateway commits response headers to the downstream client.

If a browser route fails during `streamStart`, Router can continue to the next eligible route, including an API fallback. Once a route has emitted a successful streaming response and bytes begin flowing, llmgateway does not splice a second model/provider into the middle of the same answer.

This preserves a simple invariant:

```text
failure before stream commitment -> normal route fallback
failure after stream commitment  -> stream error / partial trace
```

## Custom CDP adapters

The v0.30 true incremental implementation is enabled for built-in `browser-gemini` and `browser-qwen` adapters.

Existing custom `browser-cdp` contract-v1 scripts remain compatible. A custom v1 adapter that returns a complete `text/event-stream` string from `chat()` remains a buffered compatibility path. The gateway does not label that behavior incremental.

The additive stream primitives are intentionally compatible with contract-v1 metadata, but custom-adapter adoption is not required for v0.30.

## Deterministic CI

The v0.30 smoke fixture uses a fake loopback CDP browser that:

- starts with a managed Qwen account created while the gateway is running;
- emits three delayed browser deltas;
- proves the first data arrives before final completion;
- validates Chat, Responses and Anthropic streaming;
- drops a client connection after the first chunk;
- verifies `streamCancel` and ephemeral-target close markers;
- validates completed/cancelled Execution Trace metadata;
- forces a browser `streamStart` failure and proves API fallback still streams successfully.

No live Gemini/Qwen website is required for this release gate.
