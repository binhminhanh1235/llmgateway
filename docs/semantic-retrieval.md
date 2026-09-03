# Semantic context retrieval

llmgateway v0.7 can recover relevant verbatim excerpts from the part of a persistent thread that has already been compressed into a checkpoint.

The execution context becomes:

```text
Structured Memory IR
        +
Relevant earlier transcript excerpts
        +
Recent verbatim turns
        +
Current user turn
```

The immutable SQLite transcript remains the source of truth. Retrieval never deletes, rewrites, or promotes an excerpt into durable memory by itself.

## Why retrieval only searches checkpointed history

Recent messages are already present verbatim in the request. Searching them again would waste context and could duplicate tool exchanges. The retriever therefore searches only messages whose ordinal is less than or equal to the active checkpoint's `through_ordinal`.

This gives each context layer a clear job:

- structured memory: durable cross-turn state
- retrieved excerpts: exact older details relevant to this request
- recent messages: local conversational continuity
- current turn: highest-priority user intent

## Local-first scorer

v0.7 deliberately does not require an embedding provider. Historical messages are grouped into conversation chunks and ranked locally using a lightweight hybrid score:

- query-term overlap
- within-thread inverse document frequency, so rarer terms matter more
- repeated-term weight
- query/candidate bigram overlap
- query coverage
- a small recency tie-breaker

This works particularly well for code symbols, API names, model names, error codes, file names, architectural terms, and other concrete technical conversation details.

The scorer is isolated behind the retrieval module so an embedding or reranking implementation can replace or supplement it later without changing the thread API or Context Engine ownership model.

## Context-budget invariant

Retrieval is opportunistic. llmgateway first prepares:

```text
checkpoint + recent turns + current turn
```

Only the remaining input budget may be used for retrieved excerpts. The gateway creates an augmented candidate context and measures it again. The retrieval is committed only when the final context remains within the model-aware budget.

Therefore retrieval cannot evict the current turn or recent messages.

## Configuration

```toml
[context]
retrieval_enabled = true
retrieval_max_chunks = 3
retrieval_max_tokens = 2400
retrieval_min_score = 0.35
```

- `retrieval_enabled`: enables automatic retrieval for checkpointed persistent threads.
- `retrieval_max_chunks`: maximum historical conversation chunks injected for one turn.
- `retrieval_max_tokens`: upper bound for retrieved excerpt text before the final budget check.
- `retrieval_min_score`: minimum local relevance score required for a chunk to be considered.

## Diagnostics

Persistent thread responses include:

```text
x-llmgateway-retrieved-chunks: 0 | 1 | 2 | ...
```

To inspect retrieval without calling an LLM:

```bash
curl -X POST http://127.0.0.1:7331/v1/threads/<thread_id>/retrieve \
  -H "Authorization: Bearer $LLMGATEWAY_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "query":"How did we handle optimistic locking conflicts?"
  }'
```

Example response:

```json
{
  "thread_id": "thread_...",
  "checkpoint_through_ordinal": 42,
  "chunks": [
    {
      "start_ordinal": 11,
      "end_ordinal": 12,
      "score": 4.812,
      "preview": "user: We use optimistic locking ... assistant: Conflicts return HTTP 409 ..."
    }
  ],
  "estimated_tokens": 94,
  "rendered": "Earlier transcript ordinals 11-12: ..."
}
```

The inspector accepts optional `max_chunks`, `max_tokens`, and `min_score` overrides for diagnostics only.

## Priority and conflict handling

Retrieved excerpts are injected as supporting historical evidence. If information conflicts, the intended priority is:

```text
current/recent explicit messages
        >
structured durable memory
        >
retrieved older excerpts
```

A later explicit correction should therefore win over an old retrieved statement.

## Current limitation

The v0.7 scorer is lexical/hybrid rather than vector-embedding based. It can recover exact and near-exact technical concepts efficiently with zero extra network calls, but it will miss some paraphrases with little vocabulary overlap.

A future retriever can add local or remote embeddings, semantic reranking, provenance/confidence, and cross-thread/project retrieval while preserving the same context layers and budget rules.
