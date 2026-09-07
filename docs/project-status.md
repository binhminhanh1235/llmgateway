# Trạng thái tổng thể llmgateway

Cập nhật: **06/09/2026**

Tài liệu này là snapshot kỹ thuật của nhánh mặc định **`main`** tại thời điểm rà soát.

- code baseline đã audit: `2ae2d6d170f935ced18a4255d5a176d09b7e0fdb`
- tree baseline: `051b5f4f815b2faa0f4ffa6a4de1e923d58619bb`
- package version trong `Cargo.toml`: `0.32.0`
- trạng thái thực tế: **v0.32 + nhiều capability post-v0.32 đã merge**
- nhánh mặc định của repository: `main`, không phải `master`

> Package version chưa phản ánh hết số lượng feature đã được merge sau milestone v0.32. Vì vậy khi đánh giá khả năng của hệ thống, ưu tiên code và tài liệu snapshot này hơn nhãn version đơn lẻ.

## 1. Product direction

llmgateway là **local-first, browser-first universal LLM gateway**.

Mục tiêu là cho nhiều local client dùng một endpoint:

```text
Claude Code / Codex / OpenCode / custom client / local UI
                         |
                         v
                    llmgateway
                         |
           routing + policy + memory
                         |
       ┌─────────────────┼──────────────────┐
       v                 v                  v
 browser accounts     API accounts      model groups
```

Browser account là primary execution lane. API account vẫn được hỗ trợ như fallback/alternative khi user chủ động cấu hình.

## 2. Compatibility surface

### OpenAI-compatible

Đã có:

- `POST /v1/chat/completions`
- `POST /v1/responses`
- `GET /v1/models`
- streaming
- tool/function compatibility bridge
- strict OpenAI compatibility mode tùy chọn

### Anthropic-compatible

Đã có:

- `POST /v1/messages`
- streaming
- normalization qua common gateway request model

### Persistent conversation APIs

Đã có:

- create/list/read/delete threads
- append message
- context inspection
- memory inspection
- compaction
- retrieval

llmgateway giữ transcript canonical trong SQLite.

## 3. Conversation, memory và retrieval

Đã có:

- immutable full transcript
- Structured Memory IR
- rolling checkpoints
- compaction theo model-aware token budget
- tool-call/tool-result atomicity
- local semantic/lexical retrieval
- hybrid embedding rerank tùy chọn
- memory provenance
- retrieval diagnostics

Context pipeline:

```text
Structured Memory
      +
retrieved historical excerpts
      +
recent verbatim turns
      +
current turn
```

## 4. Routing

Đã có:

- explicit routes
- discovered account/model routes
- aliases
- virtual models
- multi-account fallback
- sticky affinity
- adaptive latency/reliability scoring
- quota/cooldown exclusion
- task-aware routing
- browser/API transport policy
- browser fairness
- recovery penalties
- route explain
- execution trace

Execution preference:

- `prefer-browser`
- `browser-only`
- `balanced`
- `prefer-api`
- `api-only`

Legacy aliases `browser-first` và `api-first` vẫn được chấp nhận.

## 5. Model groups

Model groups đã là first-class UI/config capability.

Có hai kiểu:

- flat virtual model
- ordered fallback tiers

Ví dụ:

```text
priority 10 -> GPT model
priority 20 -> Gemini model
priority 30 -> Qwen model
```

Hard tier boundary được bảo toàn: tier sau chỉ được xét sau khi tier trước không còn route eligible/successful.

Trong cùng tier vẫn áp dụng:

- readiness
- quota/cooldown
- task fit
- adaptive score
- fairness
- transport policy

## 6. Account/model/group state synchronization

Đã có cơ chế enable/disable thống nhất giữa:

- account
- account model
- model catalog
- group fallback eligibility
- `GET /v1/models`

Khi account/model bị disable:

- không được chọn cho execution;
- model có thể vẫn tồn tại trong group config;
- group đánh dấu/diễn giải model đó là ignored/ineligible;
- khi enable lại, eligibility được phục hồi nếu các điều kiện khác hợp lệ.

UI mặc định mở tab Enabled.

Account deletion lifecycle cũng đã có trong main, bao gồm backend validation và UI refresh.

## 7. Browser runtime

Đã có:

- isolated Chromium profile
- launch/verify/stop
- startup reconciliation
- stale CDP detection
- crash recovery
- reconnect sau gateway restart
- browser lifecycle state
- browser-specific readiness reason
- profile-preserving disable/stop
- interactive re-authentication
- adapter probe trước routing

Security boundary:

- không trả raw cookies qua API;
- CAPTCHA/2FA/passkey vẫn interactive;
- DevTools loopback-local;
- provider auth state được quản lý trong profile/vault.

## 8. Managed browser providers

Managed account presets hiện có:

| Provider | kind | Adapter |
|---|---|---|
| ChatGPT Web | `browser-chatgpt` | `adapters/chatgpt-web.js` |
| Gemini Web | `browser-gemini` | `adapters/gemini-web.js` |
| Qwen Web | `browser-qwen` | `adapters/qwen-web.js` |
| DeepSeek Web | `browser-deepseek` | `adapters/deepseek-web.js` |
| Xiaomi MiMo Studio Web | `browser-mimo` | `adapters/mimo-web.js` |

Ngoài ra vẫn có generic/custom browser CDP path cho trusted local adapter.

## 9. Browser streaming

Đã có:

- true incremental browser/CDP streaming
- first-byte timeout
- idle-stream timeout
- downstream backpressure
- disconnect cancellation
- provider stop/cleanup
- partial/cancelled trace metadata

Các compatibility layer OpenAI Chat, Responses và Anthropic Messages dùng chung streaming execution pipeline.

## 10. Browserless/direct transport

Post-v0.32 main đã có browserless/direct HTTP transport framework và live acceptance runners.

Mục tiêu:

```text
interactive browser login once
          |
          v
encrypted/persisted auth state
          |
          v
direct provider web backend where supported
          |
          v
Chromium can remain stopped during normal requests
```

Browserless là transport optimization, không phải authentication bypass.

Support/capability được adapter công bố và UI chỉ cho bật khi provider hỗ trợ.

Live acceptance scripts hiện có cho general browserless, Gemini model selection, Qwen và MiMo.

## 11. Provider-native conversation affinity

llmgateway vẫn sở hữu transcript canonical, nhưng một số provider adapter có thể lưu native conversation identity cho persistent threads.

Đã có infrastructure cho:

- per-thread/provider/account native identity
- reopen after runtime/tab loss
- sync cursor
- delta replay
- model binding conflict protection ở provider cần model cố định

## 12. Client policies

Đã có:

- global admin/legacy key
- per-client env-backed key
- enable/disable client
- allowed model patterns
- allowed route IDs
- execution preference
- API fallback permission
- daily/monthly request limit
- daily/monthly token limit
- persistent restart-safe enforcement
- sanitized diagnostics

Preset examples trong config dành cho:

- Claude Code
- Codex
- OpenCode
- OmniVoiceStudio

## 13. UI

Embedded local UI không cần frontend build riêng.

Các khu vực chính hiện có:

- Chat
- Accounts
- Models
- Model Groups
- routing/account diagnostics
- Trace Console
- browser runtime controls

Account UI hỗ trợ:

- add managed browser account
- login/open
- verify
- refresh models
- enable/disable account
- enable/disable model
- browserless toggle khi supported
- restart/stop/re-auth
- delete account

## 14. Testing/CI

CI hiện chạy Linux và Windows.

Linux gate gồm:

- Node syntax/UI checks
- adapter fixture tests
- shell syntax checks
- `cargo check`
- Clippy
- unit/integration tests
- nhiều smoke suites
- Docker build

Windows gate gồm:

- Rust check/test
- PowerShell runner parse
- Chromium driver smoke

Live provider acceptance tách khỏi deterministic CI để tránh phụ thuộc website/account thật.

## 15. Multimodal status

Multimodal **chưa thuộc main** tại snapshot này.

Nhánh:

`feat/multimodal-gateway`

Tại thời điểm audit so với main baseline:

- branch diverged
- ahead: 194 commits
- behind: 1 commit

Nhánh đó có code/docs đang phát triển cho:

- ArtifactStore / Files API
- general file attachments
- image/vision input
- voice/media API
- multimodal compatibility
- multimodal UI
- live acceptance runners

Các tracking issue đang mở gồm:

- #69 Multimodal Gateway
- #73 P3 General File Attachments
- #74 P4 Voice Input
- #75 P5 Image Generation
- #76 P6 Capability-aware Routing / UX
- #77 Final Regression / Live Acceptance

Không ghi các capability này là shipped trên main cho tới khi merge và verification hoàn tất.

## 16. Agent-Native P0

Agent-Native P0 đã **DONE / VERIFIED / SHIPPED trên `main`**.

Tracking:

- issue #90: closed completed;
- PR #91: merged;
- merge commit: `777c7cf3fa8b9d25a4d49ea46c2e5822e88548d3`;
- exact-head PR CI #1760 / run `34030566592`: PASS.

Main hiện có:

- portable `skills/llmgateway/SKILL.md`;
- API/routing/diagnostics/operations reference playbooks;
- helper CLI Python stdlib-only;
- helper chỉ READ + EXECUTE;
- normal execution ưu tiên scoped client key;
- admin diagnostics dùng global admin key;
- client-visible model discovery;
- logical model/group-first selection;
- explicit READ / EXECUTE / OPERATE / ADMIN boundary;
- deterministic offline helper/bundle tests trong CI.

P1 Agent Control API, P2 MCP server và P3 capability-based Agent Routing vẫn là **planned**, chưa được mô tả là shipped.

## 17. Provider Runtime Fabric

Provider Runtime Fabric đang được phát triển trên branch `feat/provider-runtime-fabric` và **chưa có trên `main`**.

Baseline:

- `f85e54b8741a8a184bdc84b142c8b770adee29c0`.

Đã verified trên working branch:

- P0 Execution Contract Foundation — DONE / VERIFIED;
- P1 Runtime Health Graph & Breakers — DONE / VERIFIED tại `9b351c707ed99fc3f3383987dc4f68bcd2eb1164`;
- P1 exact-head CI #1908 / run `34074083568` — PASS;
- P2 Account Runtime & Admission Control — DONE / VERIFIED tại `c4a663797c015183fa2e4f06f387b119794d212b`;
- P2 exact-head CI #1915 / run `34078114968` — PASS;
- P2 Linux job `101608110226` — PASS full Rust checks/tests, routing/browser/streaming/execution smoke chain và Docker;
- P2 Windows job `101608110289` — PASS check/tests/Chromium-driver smoke.

P1 hiện có provider-neutral runtime health graph theo account/transport/session, breaker CLOSED/OPEN/HALF_OPEN, bounded half-open probe, exponential cooldown + jitter, hysteresis và transport isolation. Browser-backed account có thể quarantine `direct_http` mà không tự động làm mất `browser_runtime` khỏe. Account-scoped failure vẫn giữ route cooldown compatibility.

P2 bổ sung provider-neutral per-account runtime ownership: bounded/deadline-aware admission queue, in-flight/concurrency ownership, conservative provider capability policy, adaptive concurrency, response-lifetime permits, single-flight browser lifecycle, per-session Chromium serialization, generation-safe reload/reset/stop và structured account runtime diagnostics. Overload/rejection đi qua typed common failures để Gateway reroute/fail mà Router không cần biết provider-specific transport details. P2 tiếp tục tái sử dụng P1 `RuntimeHealthGraph` thay vì tạo health system cạnh tranh.

**P2 DONE / VERIFIED trên working branch only. P3 chưa bắt đầu.** Không mô tả initiative này là shipped và không merge branch vào `main` nếu chưa có explicit approval.

## 18. Open work đáng chú ý

Tại thời điểm audit repository còn các tracking/PR mở liên quan tới:

- browserless live acceptance
- ChatGPT model picker/Sentinel recovery
- Xiaomi MiMo tracking
- multimodal initiative

Trạng thái issue/PR là tracking signal, không luôn đồng nghĩa code chưa tồn tại. Một số thay đổi có thể đã cherry-pick/merge theo commit khác trong khi PR cũ vẫn còn mở.

## 19. Điểm cần tiếp tục chuẩn hóa

Các điểm tài liệu/versioning nên tiếp tục xử lý ở release kế tiếp:

1. bump package version khỏi `0.32.0` khi chốt milestone mới;
2. chốt tên/version release cho post-v0.32 browserless/model-group/provider work;
3. đóng hoặc reconcile các PR/issue đã bị supersede;
4. merge multimodal chỉ sau final live acceptance;
5. cập nhật roadmap theo release number thực tế sau khi các gate trên hoàn tất;
6. thêm release artifacts/installation policy khi bước production distribution bắt đầu.

## 20. Source of truth

Ưu tiên theo thứ tự:

1. code trên `main`
2. current config contract
3. deterministic CI/smoke tests
4. live acceptance evidence khi feature phụ thuộc provider thật
5. docs trong `docs/`
6. tracking issue/PR metadata

Không dùng mock/fake provider test để tuyên bố live website acceptance.
