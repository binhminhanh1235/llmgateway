# Multimodal Gateway — Local Acceptance Test Plan

Status: implementation acceptance plan for `feat/multimodal-gateway`.

This plan is designed so one local run can validate the complete multimodal surface after deterministic CI is green. A live provider result is never inferred from deterministic smoke coverage.

## 1. Scope

The acceptance matrix covers:

- text chat and Threads continuity
- image input / vision
- general file attachments and native PDF upload
- ArtifactStore identity, ownership and delete protection
- audio transcription and browser microphone UX
- safe voice commands
- image generation
- image editing
- Responses image output
- capability-aware routing and explain diagnostics
- browser CDP / browserless lifecycle where applicable
- public-response leak prevention
- negative and unsupported-capability behavior

## 2. Prerequisites

Checkout the feature branch:

```bash
git checkout feat/multimodal-gateway
git pull
```

Start llmgateway with the normal local configuration and set:

```bash
export LLMGATEWAY_API_KEY='<local gateway key>'
export LLMGATEWAY_BASE_URL='http://127.0.0.1:7331'
export LLMGATEWAY_BROWSER_ACCOUNT='<authenticated ChatGPT or Gemini account id>'
```

The browser account used for vision/PDF must already be authenticated.

For the media lane, at least one enabled model must advertise each required capability:

- `audio_transcription`
- `image_generation`
- `image_editing`

The media acceptance runner discovers matching enabled models from `/v1/models`. If no model advertises a capability, that gate must fail as unsupported rather than silently routing to a text-only model.

## 3. Preflight

### T01 — Gateway health

```bash
curl -fsS "$LLMGATEWAY_BASE_URL/_llmgateway/health"
```

Pass:

- HTTP 200
- gateway reports healthy
- configured model catalog is non-empty

### T02 — Capability publication

```bash
curl -fsS "$LLMGATEWAY_BASE_URL/v1/capabilities" \
  -H "Authorization: Bearer $LLMGATEWAY_API_KEY" | python3 -m json.tool
```

Pass:

- canonical input modalities include text, image, file and audio
- gateway execution publishes image output
- `audio_transcription=true`
- `image_generation=true`
- `image_editing=true`
- attachment limits are present
- `remote_url_ingestion=false`

### T03 — Model capability discovery

```bash
curl -fsS "$LLMGATEWAY_BASE_URL/v1/models" \
  -H "Authorization: Bearer $LLMGATEWAY_API_KEY" | python3 -m json.tool
```

Pass:

- vision/file-capable models advertise their input modalities
- media models expose STT/image-generation/image-editing capabilities
- disabled account/model bindings are not presented as fallback eligible

## 4. Deterministic local smoke

Run:

```bash
bash scripts/smoke-local.sh
```

This transitively includes:

- `scripts/smoke-vision-api.sh`
- `scripts/smoke-file-attachments.sh`
- `scripts/smoke-media-api.sh`

Pass:

- exit code 0
- no upstream gateway metadata leak
- ArtifactStore create/read/delete paths pass
- capability-missing routes are excluded before scoring
- image output is persisted as gateway file IDs
- generated output does not expose provider-native IDs

## 5. One-shot live acceptance

Run:

```bash
bash scripts/live-multimodal-acceptance.sh \
  --browser-account "$LLMGATEWAY_BROWSER_ACCOUNT" \
  --base-url "$LLMGATEWAY_BASE_URL" \
  --api-key "$LLMGATEWAY_API_KEY"
```

Expected order:

1. authenticated image vision
2. authenticated native PDF/file acceptance
3. audio transcription
4. image generation
5. Responses image output
6. image editing

Pass only when the script prints:

```text
[multimodal-live] FULL LOCAL MULTIMODAL ACCEPTANCE PASS
```

A deterministic or mocked result must not substitute for the authenticated vision/PDF gates.

## 6. API acceptance scenarios

### T10 — Vision from uploaded image

Use `scripts/live-vision-acceptance.sh`.

Pass:

- image is uploaded through `/v1/files`
- chosen route actually supports image input
- provider answer depends on image contents
- authenticated provider/account is the expected one
- public response does not leak internal artifact/provider fields

### T11 — Native PDF

Use `scripts/live-file-acceptance.sh`.

Pass:

- valid PDF receives a gateway `file_*` artifact ID
- chosen route advertises native file support
- trace strategy is `native_upload`, not extracted fallback
- actual transport is browser CDP for the authenticated account
- Threads keep `llmgateway://artifact/<id>` identity
- provider-native file IDs never appear in public API responses
- follow-up text turn reuses native conversation and does not re-upload the historical PDF
- artifact delete is blocked while referenced and succeeds after reference deletion
- temporary Chromium opened by the request is released when applicable

### T12 — Text extraction fallback

Upload TXT/Markdown/CSV/JSON to a route that does not have native file upload but supports fallback.

Pass:

- bounded extraction is used
- max extracted bytes/chars are respected
- malformed JSON is rejected
- remote HTTP/HTTPS file URL ingestion is rejected

### T13 — Audio transcription

```bash
bash scripts/live-media-acceptance.sh \
  --base-url "$LLMGATEWAY_BASE_URL" \
  --api-key "$LLMGATEWAY_API_KEY" \
  --audio /path/to/real-speech.wav
```

Pass:

- audio is accepted with the correct MIME type
- transcription response contains text
- route advertises `audio_transcription`
- no provider-native file ID leaks

Recommended real speech phrase:

`Create a new chat and explain optimistic locking in one sentence.`

### T14 — Image generation

POST `/v1/images/generations`.

Pass:

- capability-aware route is selected
- one or more `file_*` output IDs are returned
- output bytes are retrievable from `/v1/files/<id>/content`
- MIME type matches actual bytes
- response does not return raw `b64_json` or provider-native IDs

### T15 — Responses image output

POST `/v1/responses` with:

```json
{
  "model": "<image-capable-model>",
  "input": "Create a compact gateway icon",
  "output_modalities": ["image"]
}
```

Pass:

- `status=completed`
- output contains `type=output_image`
- output references gateway `file_*` identity
- public response does not expose provider-native metadata

### T16 — Image editing

Use a generated artifact as the multipart `image` input to `/v1/images/edits`.

Pass:

- input image MIME is preserved
- route advertises `image_editing`
- edited output is a new gateway artifact
- original and edited artifacts remain independently retrievable/deletable

## 7. Routing and negative scenarios

### T20 — Text-only model cannot generate images

Call:

```bash
curl -fsS -X POST "$LLMGATEWAY_BASE_URL/_llmgateway/routes/explain" \
  -H "Authorization: Bearer $LLMGATEWAY_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{"model":"<text-only-model>","task":"image_generation"}'
```

Pass:

- `required_capabilities` includes `image_output`
- text-only candidate is not eligible
- exclusion contains `capability_missing:image_output`

### T21 — Missing transcription capability

Request transcription with a text-only model.

Pass:

- request fails deterministically as unsupported capability
- it must not fall through to a text/chat endpoint

### T22 — Missing image-edit capability

Request image edit with a generation-only model.

Pass:

- route is excluded before execution
- explain output includes `capability_missing:image_editing`

### T23 — Disabled model/account synchronization

Disable an account or its model binding from UI.

Pass:

- Models Enabled tab updates
- Group picker stops offering it for fallback
- routing explain marks the route ineligible
- re-enable restores fallback eligibility without deleting group membership

## 8. UI manual scenarios

Open:

```text
http://127.0.0.1:7331/
```

### U01 — Attachment controls

Pass:

- image and file attachment buttons work
- paste/drop image works
- attachment preview is visible
- model picker disables models incompatible with pending attachments
- capability detail shows input/output modalities and limits

### U02 — Voice dictation

1. Click microphone.
2. Keep mode `Dictate`.
3. Speak one sentence.
4. Stop recording.

Pass:

- browser permission is requested normally
- UI shows Listening → Transcribing → Heard
- transcript is inserted into composer
- no action is auto-executed

### U03 — Safe voice command mode

Switch to `Command` and test:

- `create new chat`
- `stop generation`
- `try again`
- `send current draft`
- `switch to model <unambiguous model>`
- `use provider <unambiguous provider>`
- `attach file <recent uploaded filename>`

Pass:

- only allowlisted actions execute
- ambiguous model/provider/artifact names do not execute
- unknown command displays a message and performs no action

### U04 — Unsafe voice commands are inert

Speak:

- `delete account primary`
- `disable model GPT`
- `change API key`
- `run shell command`
- `open arbitrary URL`

Pass:

- no admin/config/system action executes
- UI reports command not recognized/unsupported

### U05 — Generate image

1. Type an image prompt.
2. Click the image-generation control.

Pass:

- result displays as an image card
- route is visible
- Regenerate works
- output can be opened through gateway file content

### U06 — Edit generated image

Click `Edit`, enter an edit instruction.

Pass:

- output is a new image card
- original is unchanged
- route selection is capability-aware

## 9. Regression scenarios

After multimodal acceptance, verify normal text behavior:

- `POST /v1/chat/completions`
- `POST /v1/responses` text only
- `POST /v1/messages`
- Threads multi-turn continuity
- streaming
- model groups ordered fallback
- browserless/direct HTTP
- browser CDP fallback
- execution traces
- account/model enable-disable synchronization

Run the existing focused smoke suite or rely on exact-head CI for the full deterministic regression matrix.

## 10. Final acceptance record

Record:

- exact `main` SHA
- exact `feat/multimodal-gateway` SHA
- ahead / behind counts
- exact-head CI run ID and result
- local OS
- browser name/version
- authenticated browser account/provider
- vision PASS/FAIL
- native PDF PASS/FAIL
- STT PASS/FAIL
- image generation PASS/FAIL
- image editing PASS/FAIL
- capability routing PASS/FAIL
- any screenshots/log snippets for failures

The initiative can only be called fully live-verified when all required live gates pass on the same reconciled feature head, or an individual live gate is explicitly recorded as WAIVED rather than PASS.
