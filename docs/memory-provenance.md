# Memory provenance and pinning (v0.9)

v0.9 adds an item-level metadata sidecar to Structured Memory without changing the existing memory JSON contract.

## Why a sidecar

Structured Memory remains provider-independent and backward compatible:

```json
{
  "facts": [],
  "decisions": [],
  "constraints": [],
  "user_preferences": [],
  "entities": [],
  "code_context": [],
  "open_questions": [],
  "rolling_summary": ""
}
```

The new `memory_items` table tracks operational metadata for each durable item:

- stable `item_key`
- category and value
- confidence
- pinned / active state
- source kind
- first and last checkpoint ordinals
- model and route provenance
- create/update timestamps

This avoids a migration cliff for existing v0.6-v0.8 memory snapshots.

## Confidence

A checkpoint-derived item starts at confidence `0.65`.

If the same normalized item is independently retained by a later checkpoint, its confidence increases by `0.05`, capped at `0.95`. Re-reading the same checkpoint does not raise confidence.

Manual pins default to confidence `1.0`.

Confidence is metadata for inspection and future conflict/ranking policy. It does not override explicit user instructions.

## Pinning

Create a durable user-approved pin:

```bash
curl -X POST http://127.0.0.1:7331/v1/threads/<thread_id>/memory/pins \
  -H "Authorization: Bearer $LLMGATEWAY_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "category":"constraint",
    "value":"Never expose the gateway publicly without TLS",
    "confidence":1.0
  }'
```

Pin or unpin an existing memory item:

```bash
curl -X PATCH http://127.0.0.1:7331/v1/threads/<thread_id>/memory/items/<item_key> \
  -H "Authorization: Bearer $LLMGATEWAY_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{"pinned":true}'
```

Inspect the memory snapshot plus item metadata:

```bash
curl http://127.0.0.1:7331/v1/threads/<thread_id>/memory \
  -H "Authorization: Bearer $LLMGATEWAY_API_KEY"
```

## Deterministic precedence

The execution context is ordered conceptually as:

```text
checkpoint / Structured Memory
        +
pinned user-approved memory
        +
relevant retrieved history
        +
recent verbatim turns
        +
current user turn
```

Pinned memory explicitly declares that it wins over conflicting non-pinned checkpoint or retrieved history. A newer explicit user instruction still remains authoritative for the current turn.

Pins are inserted before retrieval budgeting, so semantic retrieval consumes only the context budget left after pinned durable context is included.

## Snapshot reconciliation

When a new checkpoint is produced:

- checkpoint-derived items absent from the new snapshot become inactive
- checkpoint-derived items present again remain active and may gain confidence
- pinned items are never deactivated by snapshot reconciliation
- manual pins remain `source_kind = "manual"`

The immutable transcript is unaffected.

## Diagnostics

Thread message responses include:

```text
x-llmgateway-pinned-memory: yes | no
```

This is diagnostic only. Clients do not need to understand it.
