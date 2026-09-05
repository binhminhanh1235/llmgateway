@GitHub tiếp tục triển khai trực tiếp repository `binhminhanh1235/llmgateway`, không chỉ phân tích và không hỏi lại những thông tin đã có.

## Mục tiêu duy nhất của lần chạy này

Bắt đầu và hoàn tất **P0 - Multimodal Foundation** cho initiative Multimodal Gateway.

**Không bắt đầu P1. Không merge vào `main`.**

## Source of truth

Đọc và tuân theo:

- tracking issue #69: Multimodal Gateway
- P0 issue #70
- `docs/multimodal-gateway-plan.md`
- `docs/multimodal-gateway-status.md`
- current `docs/roadmap.md`
- current browser/provider adapter contracts
- current Responses / Chat Completions / Anthropic compatibility code
- current Model Catalog / routing / client-policy contracts

Working branch family:

`feat/multimodal-gateway`

Initiative baseline main when planning started:

`46e70faf3b4a8034ca278f049f0af4b3e256e477`

**Do not assume that baseline is still current main. Re-fetch exact current `main` before editing.**

## Mandatory merge guard

Under no circumstance merge this branch, a P0 PR, or any multimodal work into `main`.

Even if:

- implementation is complete;
- CI is green;
- PR is mergeable;
- P0 is DONE / VERIFIED;
- the entire initiative later becomes DONE / VERIFIED;

you must still stop before merge unless the user gives a separate, explicit merge authorization.

## P0 scope

Implement only the provider-neutral multimodal foundation.

### 1. Canonical multimodal contracts

Add provider-neutral semantic types equivalent to:

- `Modality`
- `InputContent`
- `OutputContent`
- `MultimodalMessage`
- `MultimodalRequest`
- `MultimodalResponse`
- model/adapter structured capability types

At minimum model:

Input modalities:
- text
- image
- file
- audio

Output modalities:
- text
- image
- audio
- file

Features/limits:
- streaming
- native file upload
- image generation
- image editing
- audio transcription
- supported MIME types where known
- attachment count/size limits where known

Do not add live file upload, image upload, audio, or image generation execution yet.

### 2. Preserve compatibility

Existing text-only behavior must remain functionally unchanged.

Current public surfaces must normalize through the new canonical boundary without breaking:

- `POST /v1/responses`
- `POST /v1/chat/completions`
- Anthropic Messages
- current Threads/Responses persistence behavior
- OpenAI SDK clients
- Claude Code
- Codex
- OpenCode

For P0 it is acceptable for canonical text-only requests to be translated back into the existing OpenAI Chat Completions-shaped execution lane.

The important invariant is that public API DTOs normalize into the canonical core before provider execution.

### 3. Structured capabilities

Extend model/catalog capability representation without breaking existing capability tags.

Update `GET /v1/models` so llmgateway extension metadata exposes structured multimodal capability information.

Add:

`GET /v1/capabilities`

for gateway/provider/adapter capability diagnostics, using provider-neutral fields.

Do not hard-code frontend logic based on provider names.

### 4. Capability-aware errors

Introduce deterministic errors for unsupported multimodal requirements, such as:

- `unsupported_capability`
- `unsupported_input_modality`
- `unsupported_output_modality`

Do not return generic malformed 400 errors when the gateway can identify the actual capability mismatch.

### 5. Architectural boundary

Provider-specific DTOs, provider-native file IDs, browser selectors, upload protocols, ChatGPT/Gemini/Qwen/DeepSeek-specific types and direct/CDP transport details must not leak into the canonical multimodal contract or public API schema.

## Required tests

Add deterministic tests proving:

1. text-only Responses normalizes into canonical multimodal request;
2. text-only Chat Completions normalizes to semantically equivalent canonical content;
3. supported Anthropic text input normalizes through the same canonical boundary;
4. conversion back into the current execution request preserves existing behavior;
5. structured model capability serialization is stable;
6. legacy/string capability metadata remains backward compatible where required;
7. unsupported modality errors are deterministic;
8. existing text-only routing, streaming, browserless, browser adapters, native conversation affinity and client-policy regression gates remain green.

Run the relevant full CI/smoke suite, not only newly-added unit tests.

## Tracking discipline

Before implementation:

1. fetch exact current `main`;
2. fetch exact current `feat/multimodal-gateway` head;
3. reconcile with current main if necessary without losing planning/status commits;
4. update issue #70 from READY to IN PROGRESS;
5. update `docs/multimodal-gateway-status.md`.

During work, commit coherent changes to `feat/multimodal-gateway`.

After implementation:

1. run deterministic tests and exact-head CI;
2. record exact branch head and CI evidence;
3. update issue #70 with implemented scope, tests and evidence;
4. update status board to VERIFYING or DONE / VERIFIED only when justified;
5. keep #71 and later phases BLOCKED;
6. stop after P0 verification.

## P0 exit criteria

P0 is DONE / VERIFIED only when:

- canonical provider-neutral multimodal contracts exist;
- all current supported text APIs converge on the canonical boundary;
- existing text behavior is preserved;
- `GET /v1/models` exposes structured multimodal capability metadata;
- `GET /v1/capabilities` works;
- capability-specific errors are deterministic;
- no provider-specific details leak into the core;
- deterministic regression gates pass;
- exact-head CI is green;
- issue #70 and the status board contain final evidence.

If a live-provider test is not relevant to P0, do not invent one. Preserve existing live/runtime behavior and rely on deterministic regression gates appropriate to this contract-only phase.

## Stop condition

When P0 is DONE / VERIFIED, report:

- exact current main;
- exact P0 branch head;
- commits created;
- files/contracts changed;
- tests and CI run IDs/status;
- issue #70 status;
- remaining blockers for P1, if any.

Then stop.

**Do not start P1 and do not merge anything into main.**
