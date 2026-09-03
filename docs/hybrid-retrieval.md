# Hybrid retrieval (v0.8)

v0.8 keeps the zero-config local retriever from v0.7 and optionally adds an OpenAI-compatible embedding reranker.

## Execution model

```text
checkpointed transcript
        ↓
local lexical candidate generation
        ↓
optional embedding reranking
        ↓
context-budget filter
        ↓
retrieved historical excerpts
```

`retrieval_backend = "local"` keeps the v0.7 behavior and makes no embedding request.

`retrieval_backend = "hybrid"` uses the configured account/provider `/embeddings` endpoint. Historical chunk vectors are cached in SQLite by thread, ordinal range, embedding model, and content hash. Query vectors are intentionally not persisted.

## Graceful degradation

Embedding retrieval is an optimization layer, not a chat dependency. If the embedding call fails because of credentials, rate limits, provider outage, transport errors, or malformed responses, llmgateway logs the failure and executes the existing local retriever in the same request.

The chat request therefore remains usable even if the embedding provider is unavailable.

## Configuration

```toml
[context]
retrieval_enabled = true
retrieval_max_chunks = 3
retrieval_max_tokens = 2400
retrieval_min_score = 0.35

retrieval_backend = "hybrid"
retrieval_embedding_account = "openrouter-main"
retrieval_embedding_model = "openai/text-embedding-3-small"
retrieval_semantic_weight = 0.70
retrieval_min_similarity = 0.15
retrieval_embedding_batch_size = 64
```

The embedding account must be an enabled llmgateway account whose provider exposes an OpenAI-compatible `/embeddings` endpoint.

## Scoring

The hybrid backend combines semantic cosine similarity with the existing lexical signal:

```text
hybrid_score = semantic_weight * normalized_semantic
             + (1 - semantic_weight) * normalized_lexical
```

Cosine similarity is filtered by `retrieval_min_similarity` before a chunk is accepted. `retrieval_min_score` then applies to the combined score.

## Cache

SQLite table `retrieval_embeddings` stores vectors for historical conversation chunks. Cache identity includes:

- `thread_id`
- `start_ordinal`
- `end_ordinal`
- `embedding_model`
- content hash

If the historical chunk content changes or the embedding model changes, the vector is recomputed. Otherwise subsequent turns embed only the new query and reuse cached historical vectors.

## Diagnostics

Thread responses expose:

```text
x-llmgateway-retrieval-backend: hybrid | local | local-fallback
x-llmgateway-retrieved-chunks: <count>
```

`local-fallback` means hybrid retrieval was configured but the embedding path failed and the local retriever completed the request.

## Safety and context priority

Retrieved history remains lower priority than durable Structured Memory and recent explicit turns. Retrieval only searches the region already represented by a checkpoint and is inserted only when the final prepared input remains within the model-aware context budget.
