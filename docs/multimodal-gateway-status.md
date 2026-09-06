# Multimodal Gateway task status

Parent tracker: [#69](https://github.com/binhminhanh1235/llmgateway/issues/69)

Planning branch: `feat/multimodal-gateway`

Plan: [multimodal-gateway-plan.md](multimodal-gateway-plan.md)

Baseline main at initiative creation: `46e70faf3b4a8034ca278f049f0af4b3e256e477`

## Merge guard

**DO NOT MERGE this initiative or any phase into `main` unless BOTH conditions are true:**

1. the requested implementation is complete and verified;
2. the user gives explicit authorization to merge.

A green CI, mergeable PR, completed phase, or completed initiative is not merge authorization.

## Status legend

- `READY`: may be started now.
- `IN PROGRESS`: implementation actively underway.
- `BLOCKED`: predecessor or required gate is incomplete.
- `VERIFYING`: implementation complete; deterministic/live acceptance still running.
- `DONE / VERIFIED`: acceptance gates passed.
- `MERGE HOLD`: technically complete but waiting for explicit user merge authorization.

## Initiative board

| Phase | Task | Status | Dependency | Exit gate |
|---|---|---|---|---|
| P0 | [#70 Multimodal Foundation](https://github.com/binhminhanh1235/llmgateway/issues/70) | **DONE / VERIFIED** | none | canonical contracts + structured capabilities + compatibility tests + exact-head CI |
| P1 | [#71 ArtifactStore and Files API](https://github.com/binhminhanh1235/llmgateway/issues/71) | **DONE / VERIFIED** | P0 DONE / VERIFIED | durable files API, dedup, persistence, MIME/size/security tests |
| P2 | [#72 Image Attachment and Vision Input](https://github.com/binhminhanh1235/llmgateway/issues/72) | **DONE / VERIFIED (LIVE GATE WAIVED BY USER)** | P1 DONE / VERIFIED | deterministic/API/UI gates passed; live authenticated gate explicitly waived by user |
| P3 | [#73 General File Attachments](https://github.com/binhminhanh1235/llmgateway/issues/73) | **VERIFYING** | P2 DONE / VERIFIED | deterministic implementation gate passed; authenticated native PDF gate pending |
| P4 | [#74 Voice Input and Safe Voice Commands](https://github.com/binhminhanh1235/llmgateway/issues/74) | **BLOCKED** | P3 DONE / VERIFIED | STT + microphone + allowlisted command dispatcher |
| P5 | [#75 Image Generation and Editing](https://github.com/binhminhanh1235/llmgateway/issues/75) | **BLOCKED** | P4 DONE / VERIFIED | Responses + Images APIs share core; generation/edit verified |
| P6 | [#76 Capability-aware Routing and Multimodal UX](https://github.com/binhminhanh1235/llmgateway/issues/76) | **BLOCKED** | P5 DONE / VERIFIED | hard capability eligibility + deterministic fallback + diagnostics |
| FINAL | [#77 Final Regression and Live Acceptance](https://github.com/binhminhanh1235/llmgateway/issues/77) | **BLOCKED** | P0-P6 DONE / VERIFIED | full regression/live/security/restart matrix |

## Current checkpoint

- Branch exists: `feat/multimodal-gateway`.
- Detailed implementation plan is committed.
- Tracking issue #69 exists.
- Phase issues #70-#77 exist.
- P0 #70 is **DONE / VERIFIED**.
- Verified start checkpoint: `main` = `46e70faf3b4a8034ca278f049f0af4b3e256e477`; working branch before implementation = `77ccfea383cbebf619e05fc6993c3204bd6f17e5`; branch was ahead 3 / behind 0, so no reconcile was required.
- Re-verified before closing P0: `main` remains `46e70faf3b4a8034ca278f049f0af4b3e256e477`; P0 implementation head = `aa13c0fb7f3e3db44f86ef5238e9f75f1a207410`; branch remained behind 0.
- Canonical provider-neutral contracts are implemented in `src/multimodal.rs`: `Modality`, `InputContent`, `OutputContent`, `MultimodalMessage`, `MultimodalRequest`, `MultimodalResponse`, `ModelCapabilities`, `AdapterCapabilities`, and deterministic multimodal errors.
- `src/multimodal_compat.rs` is the common semantic boundary for Responses, Chat Completions, and Anthropic Messages. P0 text-only requests are validated canonically and then returned to the existing execution envelope unchanged.
- `GET /v1/models` retains legacy string capability tags and adds `llmgateway.multimodal_capabilities`; `GET /v1/capabilities` exposes provider-neutral model/adapter diagnostics.
- Live attachment execution remains disabled in P0. Known image/file/audio input and non-text output requests fail with deterministic capability errors rather than generic malformed-request errors.
- Deterministic tests cover Responses normalization, Chat/Responses equivalence, Anthropic normalization, current execution round-trip semantics, stable structured capability serialization, legacy capability compatibility, and unsupported modality errors.
- Full implementation exact-head CI: workflow CI #1345 / run `33975834628` on `aa13c0fb7f3e3db44f86ef5238e9f75f1a207410`: **PASS** on Rust + Windows, including strict cargo check, Clippy, all-target tests, OpenAI SDK, browser/browserless, streaming, native affinity, routing/traces, client policies, local multimodal assertions, and Docker build.
- The final status-only checkpoint commit is evidence-only. Its exact-head CI run is recorded in issue #70 after completion so no code-evidence commit is mutated merely to embed its own future run ID.
- P1 #71 is **DONE / VERIFIED**. P2 #72 is **DONE / VERIFIED (LIVE GATE WAIVED BY USER)**. P3 #73 is **VERIFYING** after its deterministic exact-head implementation gate passed. P4 and every later phase remain **BLOCKED / NOT STARTED**.
- `main` remains untouched. No multimodal work may be merged to `main` without a separate explicit user authorization.

## Update protocol

At every meaningful checkpoint:

1. verify exact current `main`;
2. verify exact working branch head;
3. update the relevant phase issue status;
4. update this status board when phase state changes;
5. record deterministic CI run IDs and live acceptance evidence;
6. do not mark a phase DONE / VERIFIED from stale CI after `main` changes;
7. do not start the next phase before the previous phase is DONE / VERIFIED;
8. never merge to `main` without explicit user authorization.

- P1 start checkpoint: `main` = `46e70faf3b4a8034ca278f049f0af4b3e256e477`; branch = `64d5f53f9e0da182d3dfc0a7e4f1d3b948ba94e8`; ahead 8 / behind 0; no reconcile required.


## P1 ArtifactStore and Files API verification

- Exact main re-check: `46e70faf3b4a8034ca278f049f0af4b3e256e477`.
- P1 implementation head: `d5d008d16a9b0781951484795ac177dfbd681a2b`; branch is behind main by 0 commits.
- Durable provider-neutral ArtifactStore is implemented in `src/artifact_store.rs` using the shared SQLite database plus a configurable data-root-backed content-addressed blob store.
- Artifact metadata includes logical file id, owner client id, safe filename, MIME type, size, SHA-256, purpose/source, lifecycle state and timestamps. Public Files API responses never expose absolute or relative filesystem paths.
- SHA-256 content addressing deduplicates identical physical blobs while preserving distinct logical file records.
- Persistent tables cover artifact blobs, artifacts, artifact references and provider artifact bindings. Reference guards block unsafe delete; provider bindings are deterministically removed with the logical artifact; startup cleanup removes orphaned blob rows/files.
- Authenticated Files API is wired at `POST /v1/files`, `GET /v1/files/{file_id}`, `GET /v1/files/{file_id}/content`, and `DELETE /v1/files/{file_id}`.
- Client ownership isolation is enforced: a client cannot read another client's artifact; the API returns a non-leaking 404. Admin-key access remains global.
- Multipart upload is bounded at the Axum body layer and again by exact per-file/total request limits before artifact persistence.
- MIME sniffing validates declared content and rejects configured dangerous executable MIME types. Configurable MIME allow/deny lists and attachment limits are exposed through `GET /v1/capabilities`.
- Remote URL ingestion remains disabled and configuration rejects enabling it until SSRF controls exist in a later explicitly scoped phase.
- Unit acceptance covers deduplication, metadata/content survival after ArtifactStore restart, ownership isolation, reference-safe delete, provider-binding cleanup, oversize rejection, MIME mismatch and executable rejection before metadata persistence.
- API smoke acceptance covers unauthenticated rejection, upload/metadata/content round-trip, cross-client isolation, duplicate physical-blob count, MIME spoof rejection, oversize rejection, dedup-safe partial delete and final blob cleanup.
- Implementation CI #1402 / run `33977856797` on exact implementation head `d5d008d16a9b0781951484795ac177dfbd681a2b`: **PASS** on Rust + Windows, including strict `-D warnings`, Clippy, all-target tests, Files API smoke, all existing browser/browserless/streaming/routing/client-policy smokes and Docker build.
- This status-only evidence commit requires one final exact-head CI. Its run ID will be recorded on issue #71 after completion, without mutating the verified branch head merely to embed its own future run ID.
- P2 #72 and all later phases remain **BLOCKED / NOT STARTED**. No P2 work has begun.
- `main` remains untouched. No multimodal work is merged without separate explicit user authorization.


- P2 start checkpoint: `main` = `46e70faf3b4a8034ca278f049f0af4b3e256e477`; branch = `7f3712f46e6a88163991bd662a961046d51ff8f6`; ahead 21 / behind 0; no reconcile required.
- P1 final exact-head CI #1403 / run `33978273008`: **PASS**.


## P2 Image Attachment and Vision Input verification

- Exact main remains `46e70faf3b4a8034ca278f049f0af4b3e256e477`.
- Deterministic P2 implementation head: `e47377a0264eff922a8b0435bb7cc4143bb7034c`; branch remained behind main by 0 commits.
- Canonical vision input resolves data URLs and gateway `file_id` references through ArtifactStore, persists stable `llmgateway://artifact/<file_id>` references and materializes provider payloads only at the execution boundary.
- Chat Completions and Responses accept image input; Responses can reuse a previously uploaded gateway artifact. Unsupported file/audio input remains deterministic and is not silently collapsed by compatibility translation.
- Threads persist image artifact references, rematerialize them for provider execution and hold reference guards so referenced files cannot be deleted until the owning thread is deleted.
- Image routing rejects unsupported API routes deterministically. Verified ChatGPT/Gemini browser adapters can supply vision capability for legacy account/route metadata without falsely enabling non-browser API providers.
- ChatGPT Web and Gemini Web CDP adapters attach image bytes through real file inputs before submit. Direct/browserless transports do not claim image support. Browserless-preferred image turns temporarily fall back to CDP when required and restore a previously closed browser posture after the turn.
- UI supports picker, drag/drop, pasted screenshots, preview/remove and model-capability gating. Selected images upload through `/v1/files` first and chat state sends stable `file_id` references rather than storing base64 blobs.
- Deterministic browser fixtures verify attach-before-submit for ChatGPT/Gemini. `scripts/test-vision-ui.mjs` locks the composer image contract.
- `scripts/smoke-vision-api.sh`, invoked from `scripts/smoke-local.sh`, verifies inline image Chat Completions, stored-image upload, Responses reuse, a text-only rejection, Threads stable-reference persistence, `artifact_in_use` protection and cleanup.
- Exact implementation CI #1488 / run `34001876132` on `e47377a0264eff922a8b0435bb7cc4143bb7034c`: **PASS**. Linux passed strict `-D warnings`, Clippy, all-target tests, all P2 and regression smokes, client policies and Docker. Windows passed cargo check/test and Chromium-driver smoke.
- Live authenticated acceptance runner: `scripts/live-vision-acceptance.sh`. It is syntax-gated in CI and is designed for an already authenticated ChatGPT/Gemini browser account without storing credentials in the repository.
- P2 final deterministic exact-head CI #1524 / run `34003689302` on `11310a65e92ef594fa95ef4bbf0199b842b3bf5a`: **PASS**, including live-runner syntax, vision API/UI fixtures, Model Groups, Linux/Windows regressions and Docker.
- The only unexecuted P2 gate was the real authenticated ChatGPT/Gemini local vision run. On 2026-09-06 the user explicitly instructed to **ignore that live authenticated execution and continue**. This is recorded as a user waiver, not as a claimed live PASS.
- P2 is therefore **DONE / VERIFIED (LIVE GATE WAIVED BY USER)** for dependency progression.
- P3 #73 is **VERIFYING** after deterministic implementation CI passed. The real authenticated native-PDF acceptance remains pending. P4 and later phases remain **BLOCKED / NOT STARTED**.
- P3 start checkpoint: `main` = `c8dd755ea126b12842081035d7654d66efa0e81a`; pre-start feature head = `11310a65e92ef594fa95ef4bbf0199b842b3bf5a`; branch ahead 70 / behind 0, so no reconcile is required.
- `main` remains untouched and the merge guard remains active.


## P3 General File Attachments verification

- Exact current main re-check before the verification checkpoint: `71436138b0fe5d67b93dac5c0e68b1ad112ac7c2`.
- P3 deterministic implementation head: `fc8fc8d82b9bb85bfb959d59288b15ed3c94df73`; branch is ahead 135 / behind 0 relative to that exact main, so no reconcile is required.
- Exact-head CI #1654 / run `34010591132`: **PASS** on Linux and Windows, including strict cargo check, Clippy, all-target tests, provider-conversation affinity, browser/browserless regression smoke, routing/policy smoke, file UI fixtures, live-runner syntax and Docker build.
- The P3 execution lane now supports native provider document upload for PDF/DOCX and bounded extraction fallback for TXT/Markdown/CSV/JSON. Unsupported remote file URLs and unsupported MIME/route combinations fail deterministically.
- Provider artifact bindings are wired into successful production execution and isolated by gateway artifact + provider + account affinity. Same-affinity bindings are reused as stable gateway-internal opaque bindings; provider/account switches create distinct bindings without mutating the gateway artifact. Raw provider file IDs remain outside public API payloads.
- File-capable model, route and adapter capability metadata now exposes supported file MIME types plus `max_attachment_count` and `max_attachment_size_bytes`. Deterministic smoke coverage locks the route-level metadata contract.
- Threads and Responses persist stable `llmgateway://artifact/<id>` references. Native conversation replay keeps already-synced file turns out of incremental provider requests, while unsynced file turns remain replayable.
- Extraction guardrails are explicitly tested for byte limits, character limits and invalid JSON.
- `scripts/live-file-acceptance.sh` provides the P3 authenticated acceptance gate for ChatGPT/Gemini browser adapters: valid PDF upload, Responses, Threads, native CDP transport, trace strategy, stable artifact identity, leak checks, reference-safe delete and follow-up reuse semantics.
- Remaining P3 blocker: execute the real authenticated native-PDF acceptance against at least one supported browser account. This gate has **not** been waived and is **not** claimed as a live PASS.
- P4 #74 remains **BLOCKED / NOT STARTED** until P3 reaches DONE / VERIFIED.
- `main` remains untouched and the merge guard remains active.
