# Provider-native conversation affinity

llmgateway persistent threads can bind to provider-native browser conversations without making the provider the source of truth.

Native affinity is supported by the built-in Gemini Web and ChatGPT Web providers.

## Semantics

For the thread API:

```text
llmgateway thread A
        |
        +-- Gemini account A  -> native Gemini conversation X
        +-- ChatGPT account B -> native ChatGPT conversation C

llmgateway thread B
        |
        +-- Gemini account A  -> native Gemini conversation Y
        +-- ChatGPT account B -> native ChatGPT conversation D
```

Continuing a thread reopens the provider-native conversation mapped to that account. Creating a new llmgateway thread creates and records a separate native conversation.

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

When a native-affinity browser route handles the first turn of a logical thread for an account:

1. llmgateway has no provider mapping yet.
2. The browser provider opens the provider's new-chat URL.
3. The adapter sends the prepared llmgateway context once as bootstrap context.
4. The provider creates its native conversation and changes the page URL.
5. llmgateway observes the navigated CDP target URL and persists it.
6. After the assistant response is stored, llmgateway advances the provider sync cursor.

The provider tab can then be closed. The native conversation itself remains available on the provider.

## Subsequent turns

When the same logical thread is routed to the same browser account:

1. llmgateway loads the stored provider conversation URL.
2. If the mapped provider tab is still open, llmgateway reuses that exact CDP target.
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

A persistent llmgateway thread keeps its provider tab alive while the Chromium runtime is alive. Repeated turns in the same llmgateway chat therefore use the same browser target and the same native conversation.

A new llmgateway chat opens a different provider tab and records a different native conversation.

If the mapped tab is no longer present, llmgateway falls back to the persisted native conversation URL, reopens it once, and then keeps reusing that replacement tab. Stateless compatibility requests retain the existing ephemeral-tab behavior.

## Provider constraints

The current native-affinity implementation is enabled for `browser-gemini` and `browser-chatgpt`.

The provider conversation URL must resolve to the configured provider origin and a recognized native chat shape. Gemini account-scoped paths such as `/u/1/app/<chat-id>` and Gem paths are normalized by native chat identity. ChatGPT conversations are normalized by the conversation ID following the `/c/` path segment, including URLs such as `/c/<chat-id>` and `/g/<gpt-id>/c/<chat-id>`.

If a provider changes its native conversation URL shape, the adapter may need a compatibility update. The logical llmgateway thread history remains intact even if the provider mapping becomes unusable.
