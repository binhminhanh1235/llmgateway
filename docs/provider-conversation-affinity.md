# Provider-native conversation affinity

llmgateway persistent threads can bind to provider-native browser conversations without making the provider the source of truth.

The initial implementation targets the built-in Gemini Web provider.

## Semantics

For the thread API:

```text
llmgateway thread A
        |
        +-- Gemini account A -> native Gemini conversation X

llmgateway thread B
        |
        +-- Gemini account A -> native Gemini conversation Y
```

Continuing thread A reopens conversation X. Creating thread B creates and records a different Gemini conversation.

The ordinary stateless compatibility endpoints keep their existing behavior:

```text
POST /v1/chat/completions
POST /v1/responses
POST /v1/messages
```

Those requests do not receive a logical llmgateway thread ID and therefore continue to use stateless browser execution.

## Persistence model

The conversation database contains a provider mapping keyed by:

```text
thread_id
provider
account_id
```

Each row stores:

```text
conversation_url
last_synced_ordinal
created_at
updated_at
```

The row belongs to the llmgateway thread through a foreign key with `ON DELETE CASCADE`.

llmgateway remains the canonical conversation store. The provider-native conversation is an execution surface and an optimization for browser continuity.

## First turn

When a Gemini route handles the first turn of a logical thread for an account:

1. llmgateway has no provider mapping yet.
2. The browser provider opens Gemini's new-chat URL.
3. The adapter sends the prepared llmgateway context once as bootstrap context.
4. Gemini creates its native conversation and changes the page URL.
5. llmgateway observes the navigated CDP target URL and persists it.
6. After the assistant response is stored, llmgateway advances the provider sync cursor.

The provider tab can then be closed. The native Gemini conversation itself remains available on the provider.

## Subsequent turns

When the same logical thread is routed to the same Gemini account:

1. llmgateway loads the stored Gemini conversation URL.
2. If the mapped Gemini tab is still open, llmgateway reuses that exact CDP target.
3. If the tab disappeared because Chromium or the gateway restarted, llmgateway opens the stored native conversation URL once and keeps that replacement tab.
4. llmgateway sends only provider-missed stored turns plus the current user turn.
5. The assistant response is persisted locally.
6. The mapping's `last_synced_ordinal` advances.

This avoids replaying the whole conversation into a native thread that already contains it.

## Account failover

Mappings are account-specific.

If thread A normally uses Gemini account A but fails over to account B:

```text
thread A
  +-- account A -> Gemini X
  +-- account B -> Gemini Z
```

Account B has no mapping on its first use, so llmgateway bootstraps the current prepared context and records Gemini Z.

If routing later returns to account A, llmgateway compares X's sync cursor with the stored local thread history and sends only the turns account A missed before sending the current user turn.

This preserves browser-account failover while keeping the logical conversation stable.

## Tab affinity

A persistent llmgateway thread keeps its Gemini tab alive while the Chromium runtime is alive. Repeated turns in the same llmgateway chat therefore use the same browser target and the same native Gemini conversation.

A new llmgateway chat opens a different Gemini tab and records a different native conversation.

If the mapped tab is no longer present, llmgateway falls back to the persisted native conversation URL, reopens it once, and then keeps reusing that replacement tab. Stateless compatibility requests retain the existing ephemeral-tab behavior.

## Provider constraints

The current native-affinity implementation is enabled for `browser-gemini`.

The provider conversation URL must resolve to the configured Gemini origin and a recognized native chat shape. Account-scoped paths such as `/u/1/app/<chat-id>` and Gem paths are normalized by native chat identity before reuse, so llmgateway does not treat equivalent Gemini URLs as different conversations.

If Gemini changes its native conversation URL shape, the adapter may need a compatibility update. The logical llmgateway thread history remains intact even if the provider mapping becomes unusable.
