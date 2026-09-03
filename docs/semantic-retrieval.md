# Semantic Context Retrieval (v0.7)

v0.7 adds retrieval of relevant historical excerpts from the part of a persistent thread already represented by a Context Engine checkpoint.

## Context compiler shape

```text
Structured Memory
      +
Retrieved historical excerpts
      +
Recent verbatim messages
      +
Current user turn
      ↓
Selected model/account route
```

The full transcript remains the source of truth in SQLite. Retrieval never deletes, rewrites, or reorders stored messages.

## Why retrieval only targets checkpointed history

Recent messages already travel verbatim in the prepared context. Searching them again would duplicate tokens. Retrieval therefore searches messages at or before the active checkpoint's `through_ordinal`, rehydrating details that the compressed memory may have omitted.

## Scorer abstraction

The retrieval core exposes a `RetrievalScorer` trait. v0.7 ships `LexicalScorer` (`lexical-v1`) as the zero-dependency local implementation. It uses normalized term overlap plus a small recency weight.

An embedding scorer can later implement the same interface without changing thread APIs or the Context Compiler contract.

## Safe chunking

Historical messages are grouped by conversation turn. Retrieved chunks are rendered as quoted system context rather than replayed as raw assistant/tool messages. This avoids creating orphan tool calls or tool results when a historical fragment is injected.

## Budget behavior

Retrieval uses only remaining context-budget slack after the normal checkpoint + recent-turn context has been prepared. It never displaces the current user turn. If there is insufficient slack, zero retrieval chunks are injected.

## Configuration

```toml
[context]
retrieval_enabled = true
retrieval_top_k = 4
retrieval_max_tokens = 2000
retrieval_min_score = 0.08
```

- `retrieval_enabled`: enable historical retrieval for checkpointed threads.
- `retrieval_top_k`: maximum number of candidate chunks after ranking, capped at 20.
- `retrieval_max_tokens`: retrieval-specific token ceiling before the final context-budget slack check.
- `retrieval_min_score`: minimum lexical relevance score from 0 to 1.

## Inspect retrieval

```bash
curl "http://127.0.0.1:7331/v1/threads/<thread_id>/retrieval?q=optimistic%20locking" \
  -H "Authorization: Bearer $LLMGATEWAY_API_KEY"
```

Example:

```json
{
  "thread_id": "thread_...",
  "through_ordinal": 42,
  "scorer": "lexical-v1",
  "data": [
    {
      "start_ordinal": 11,
      "end_ordinal": 12,
      "score": 0.83,
      "text": "user: ...\nassistant: ..."
    }
  ]
}
```

## Response diagnostics

Persistent thread responses include:

```text
x-llmgateway-retrieved-chunks: <number>
```

This is diagnostic only. Clients can ignore it.

## Guarantees

1. Retrieval only searches the older checkpointed transcript during normal thread execution.
2. Recent messages and the current user turn take precedence over retrieved excerpts.
3. Retrieved history is quoted context, not executable tool history.
4. Retrieval consumes only available context-budget slack.
5. No external embedding service or additional credential is required in v0.7.
6. The scorer interface is intentionally replaceable by a future embedding/hybrid scorer.
