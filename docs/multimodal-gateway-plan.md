# Multimodal Gateway implementation plan

## Status

Planning branch: `feat/multimodal-gateway`

Exact base main: `46e70faf3b4a8034ca278f049f0af4b3e256e477`

This branch is intentionally isolated from `main`. The work should be delivered in small guarded PRs/phases rather than one large merge.

## Goal

Evolve llmgateway from a text-focused LLM gateway into an OpenAI-compatible multimodal gateway where the same core supports:

- text input/output;
- image attachments / vision input;
- general file attachments;
- microphone input and speech-to-text;
- local/UI voice commands;
- image generation and image editing;
- future audio/image/file outputs;
- browser-backed, browserless/direct HTTP, API-key, and future local adapters;
- UI and API clients through the same canonical execution path.

The public API and the local UI are equal first-class clients. Provider-specific DTOs, browser selectors, upload protocols, and web transport details must not leak into the public API layer.

## Current architectural constraints

At the base SHA:

- `POST /v1/responses` is implemented as a compatibility translation into the current OpenAI Chat Completions-shaped execution path;
- `POST /v1/chat/completions` and Anthropic Messages share the existing routed execution machinery;
- persistent Responses state and Threads/conversation state already exist;
- Model Catalog already exposes route/model capability metadata through `GET /v1/models`;
- browser adapters already have a versioned contract plus dynamic model discovery, transport capabilities, native conversation affinity, browserless/direct HTTP and CDP execution;
- Gemini, ChatGPT, Qwen and DeepSeek Web are first-class browser providers;
- request-level client policies, budgets, route/model allowlists and transport boundaries already exist.

The multimodal design must extend these contracts rather than bypass them.

---

# Architectural invariants

## 1. One canonical multimodal core

All public surfaces normalize into one internal request type:

```text
Chat UI
OpenAI Responses
OpenAI Chat Completions
Threads API
Images API
Audio API
        |
        v
Public API normalizers
        |
        v
Canonical MultimodalRequest
        |
        v
Capability-aware Router
        |
        v
Adapter capability execution
```

No API endpoint gets its own provider-specific execution pipeline.

## 2. Artifact IDs are the stable attachment identity

Images, documents and audio are stored as gateway artifacts.

Inputs accepted by API/UI may originate from:

- multipart upload;
- existing `file_id`;
- data URL/base64;
- remote URL when explicitly enabled.

They normalize to an internal `ArtifactId` before adapter execution.

## 3. Provider file bindings are cacheable execution details

A gateway artifact may be uploaded natively to several provider accounts:

```text
file_123
  -> chatgpt-web/account-a/provider-file-x
  -> gemini-web/account-b/provider-file-y
```

Bindings are keyed by provider + account and may be reused when safe. The API client never needs to know provider-native file IDs.

## 4. Capabilities are model/account/adapter facts

Capability metadata must distinguish at least:

- input modalities: text, image, file, audio;
- output modalities: text, image, audio, file;
- native file upload;
- image generation;
- image editing;
- audio transcription;
- streaming;
- supported MIME types;
- max attachment count / size when known.

UI and routing consume the same capability metadata. No frontend provider-name branching.

## 5. Compatibility remains intact

Existing text-only clients, including OpenAI SDK, Claude Code, Codex and OpenCode, must continue to work unchanged.

Multimodal additions must not change the behavior of existing text-only routes unless a new capability is explicitly requested.

## 6. Security boundaries stay strict

- browser credentials/cookies remain private to authenticated profile/runtime;
- file APIs require the same client authentication/policy boundary as other public APIs;
- maximum body/file sizes are enforced before persistence;
- MIME type is validated/sniffed rather than trusting filename alone;
- remote URL fetching is disabled by default or protected against private-network/loopback SSRF;
- artifacts never expose arbitrary filesystem paths;
- deleting an artifact must not delete unrelated provider conversation state;
- provider terms, quota and anti-abuse controls remain respected.

---

# Proposed canonical types

The exact Rust shape can change during implementation, but the semantic contract should resemble:

```rust
enum Modality {
    Text,
    Image,
    File,
    Audio,
}

enum InputContent {
    Text { text: String },
    Image { artifact_id: ArtifactId },
    File { artifact_id: ArtifactId },
    Audio { artifact_id: ArtifactId },
}

enum OutputContent {
    Text { text: String },
    Image { artifact_id: ArtifactId },
    Audio { artifact_id: ArtifactId },
    File { artifact_id: ArtifactId },
    ToolCall { call: ToolCall },
}

struct MultimodalRequest {
    model: ModelSelector,
    messages: Vec<MultimodalMessage>,
    output_modalities: Vec<Modality>,
    routing: Option<RoutingRequirements>,
}
```

The core must not contain OpenAI-specific names such as `image_url` or provider-native upload IDs.

---

# Public API target

## Canonical

- `POST /v1/responses`
- `GET /v1/models`

## Compatibility / convenience

- `POST /v1/chat/completions`
- existing Threads endpoints extended with multimodal message content;
- `POST /v1/files`
- `GET /v1/files/{file_id}`
- `GET /v1/files/{file_id}/content`
- `DELETE /v1/files/{file_id}`
- `POST /v1/images/generations`
- `POST /v1/images/edits`
- `POST /v1/audio/transcriptions`
- optional later: `POST /v1/audio/speech`
- `GET /v1/capabilities` for gateway/adapter capability diagnostics.

The convenience endpoints must normalize into the same multimodal core used by `/v1/responses`.

---

# Delivery plan

## P0 - Multimodal contract foundation

### Scope

Introduce provider-neutral core contracts without enabling live attachments yet.

Work:

- add `Modality`, `InputContent`, `OutputContent`, `MultimodalMessage`, `MultimodalRequest`, `MultimodalResponse`;
- introduce structured `ModelCapabilities` / `AdapterCapabilities`;
- preserve existing string capability tags for migration compatibility where needed;
- teach `GET /v1/models` to expose structured multimodal capability metadata under the llmgateway extension object;
- add `GET /v1/capabilities`;
- create public API normalizer boundaries:
  - Responses -> multimodal core;
  - Chat Completions -> multimodal core;
  - Anthropic -> multimodal core where supported;
- initially convert canonical text-only requests back into the existing execution representation so behavior is unchanged;
- define capability-aware error types such as `unsupported_capability`, `unsupported_input_modality`, `unsupported_output_modality`.

### Acceptance

- all existing text-only smoke/CI gates remain green;
- identical text requests produce equivalent upstream requests/results before and after P0;
- `/v1/models` returns structured capability metadata without breaking existing clients;
- no provider-specific DTO exists in the canonical core;
- deterministic tests prove API normalizers converge on the same canonical request.

---

## P1 - ArtifactStore and Files API

### Scope

Add durable first-class artifacts.

Work:

- create artifact metadata persistence in SQLite;
- create configurable data-root-backed blob storage;
- SHA-256 content addressing/deduplication;
- artifact metadata:
  - id;
  - filename;
  - MIME type;
  - size;
  - SHA-256;
  - created_at;
  - purpose/source;
  - lifecycle state;
- implement authenticated Files API;
- multipart upload with bounded streaming/body size;
- safe content download;
- delete semantics with reference checks/soft-delete policy;
- provider binding table for future native uploads;
- artifact references in persisted response/thread state;
- config limits for file count, per-file size and total request size;
- MIME allow/deny policy;
- optional remote URL ingestion contract, disabled by default until SSRF controls are implemented.

### Acceptance

- upload -> metadata -> content -> delete round-trip works;
- identical uploads deduplicate blob content;
- invalid/oversized files fail before unsafe persistence;
- artifact metadata survives restart;
- APIs never expose absolute local paths;
- client policies/auth apply to Files API;
- orphan/provider-binding cleanup has deterministic tests.

---

## P2 - Image attachment / vision input

### Scope

Deliver the first real multimodal execution lane.

API:

- `/v1/responses` supports `input_image` via `file_id`, supported URL/data URL forms;
- `/v1/chat/completions` supports OpenAI-style multimodal content;
- Threads messages can persist image artifact references.

UI:

- image picker;
- drag/drop and paste screenshot;
- preview/remove before send;
- capability-aware disabled state when selected model cannot accept images.

Adapters:

- add an image-input capability contract;
- implement provider-native/direct HTTP or CDP upload path only where verified;
- preserve per-account native conversation affinity;
- cache provider artifact binding when safe;
- route fails with explicit capability error rather than malformed prompts.

Initial adapter order should be based on verified provider behavior, preferably ChatGPT Web and Gemini Web first, then Qwen/others when supported.

### Acceptance

- one image + text works through API and UI for at least one real adapter;
- the same stored image can be reused in a later request;
- unsupported models return deterministic `unsupported_capability`;
- browser-only and browserless/direct transport policy still works as configured;
- existing text streaming remains unchanged;
- deterministic fake-provider fixtures cover image upload/request construction.

---

## P3 - General file attachments

### Scope

Support PDFs and common documents without coupling API clients to provider upload APIs.

Initial types:

- PDF;
- TXT/Markdown;
- DOCX;
- CSV/JSON where reasonable.

Execution strategies:

1. native provider file upload when available;
2. gateway extraction fallback when native upload is unavailable and the MIME type has a safe extractor;
3. explicit unsupported error when neither path exists.

Add:

- provider file binding reuse;
- capability metadata for supported MIME types and limits;
- attachment references in Threads/Responses persistence;
- UI file chips/progress/error state;
- extraction size/token guardrails;
- clear trace metadata: native upload vs extracted fallback.

### Acceptance

- PDF works end-to-end for at least one native-upload adapter;
- one fallback extractor path is covered deterministically;
- switching provider/account either reuses a valid binding or creates a new provider binding without corrupting the gateway artifact;
- no raw provider file IDs leak through public responses;
- thread replay does not duplicate file upload unnecessarily.

---

## P4 - Voice input and voice commands

### P4A Speech-to-text

API:

- `POST /v1/audio/transcriptions`.

UI:

- microphone record/stop;
- visible recording state;
- transcript preview before send.

Provider strategy:

- define `AudioTranscriptionCapability`;
- allow local STT backend in the future;
- provider-backed STT is optional and capability-gated.

### P4B Voice commands

Voice commands are a UI/gateway control plane, not ordinary prompts unless the user chooses dictation.

Initial commands:

- new thread;
- stop generation;
- retry/regenerate;
- switch model/provider when unambiguous;
- send current draft;
- attach/select already-known local artifact only through safe UI interaction.

Recognition produces a typed command object such as:

```json
{
  "command": "set_model",
  "model": "..."
}
```

The command dispatcher must have an allowlist. Free-form voice text must never become arbitrary admin/API operations.

### Acceptance

- recorded audio can become transcript and then a normal multimodal request;
- voice command actions are visibly distinguishable from dictated prompt text;
- stop/retry/new-thread command path has deterministic UI tests;
- no voice phrase can invoke an unregistered privileged command.

---

## P5 - Image generation and editing

### API

- `POST /v1/images/generations`;
- `POST /v1/images/edits`;
- `POST /v1/responses` with image output modality.

### Core

Add image-output events/results to the canonical response stream.

Generated images are persisted to ArtifactStore and returned through gateway file identities.

### UI

- capability-filtered image model selection;
- image output cards;
- regenerate;
- edit using previous generated/uploaded image;
- size/aspect/quality options only when supported by the selected provider.

### Acceptance

- text -> image works through both `/v1/images/generations` and `/v1/responses` using the same internal execution path;
- image + edit prompt -> new image artifact works for one verified adapter;
- generated output survives restart and can be referenced by later messages;
- unsupported provider options are rejected/omitted rather than guessed;
- streaming emits at least started/done events, with progress only when the provider genuinely supplies it.

---

## P6 - Capability-aware auto routing and multimodal UX completion

### Scope

Extend routing eligibility before scoring:

```text
requested modalities/features
        |
        v
hard capability filter
        |
        v
client policy / transport boundary
        |
        v
readiness / quota / affinity
        |
        v
existing scoring/fairness
```

Support `model=auto` or the existing virtual-model mechanism with explicit requirements such as:

- image input required;
- file input required;
- image output required;
- transcription required.

Keep client policy rules authoritative: automatic routing may narrow eligible routes but never broaden a client's configured permissions.

UI:

- capability badges;
- model picker filtering;
- attachment limits;
- Accounts capability matrix;
- diagnostics explaining why a route was excluded.

### Acceptance

- text+image never selects a text-only route;
- image generation never selects a text-only output model;
- client route/model/transport policy remains a hard boundary;
- route explain shows capability exclusion reasons;
- affinity is retained only while the bound route remains eligible;
- fallback to another provider is deterministic when capability/readiness changes.

---

# Optional follow-up after P6

Not required for the first multimodal milestone:

- TTS via `POST /v1/audio/speech`;
- audio output in Responses;
- video input;
- richer document extraction/RAG;
- artifact retention quotas/GC policies;
- signed local download URLs if remote clients are introduced;
- native SDK helpers.

---

# Testing strategy

Every phase requires deterministic CI before live-provider acceptance.

Layers:

1. Rust unit tests for canonical contracts, validation and capability matching;
2. API compatibility tests for OpenAI Responses/Chat Completions/Files/Images/Audio shapes;
3. SQLite restart/migration tests;
4. fake-provider/direct-HTTP/CDP fixtures;
5. UI tests for composer state, uploads, microphone and capability filtering;
6. live authenticated acceptance only after deterministic gates are green.

Existing text-only browser streaming, model discovery, native conversation affinity, client-policy and browserless smoke tests are regression gates throughout all phases.

---

# Suggested PR sequence

Do not implement all phases in one branch-sized PR.

Recommended PR train:

1. P0 contract + compatibility foundation;
2. P1 ArtifactStore + Files API;
3. P2 image input;
4. P3 general files;
5. P4 voice input/commands;
6. P5 image generation/editing;
7. P6 capability-aware routing/UX.

Each PR should rebase/reconcile against exact current `main`, run exact-head CI, and preserve the previous phase's live/deterministic gates.

---

# Definition of done for the multimodal initiative

The initiative is DONE only when:

- UI and API use the same canonical multimodal core;
- text-only compatibility remains green;
- image and file attachments work through API and UI;
- microphone input/transcription works;
- safe voice commands work;
- image generation works through both Responses and Images APIs;
- Threads/persistent Responses preserve artifact references;
- provider-native artifact IDs remain internal;
- model/account/adapter capability metadata is observable;
- auto routing excludes incompatible routes before scoring;
- browserless/direct HTTP and CDP transport semantics remain intact;
- security/file-size/MIME/SSRF boundaries are tested;
- all deterministic CI plus the selected live-provider acceptance matrix are green.
