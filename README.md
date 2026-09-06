# llmgateway

**llmgateway** là một universal LLM gateway chạy local, viết bằng Rust, tập trung vào mô hình **browser-first**: dùng các tài khoản LLM đã đăng nhập trên trình duyệt như ChatGPT, Gemini, Qwen, DeepSeek hoặc Xiaomi MiMo làm execution backend, đồng thời vẫn hỗ trợ API provider khi cần.

> Một endpoint local, nhiều provider/model/account, có routing, fallback và conversation state thống nhất.

## Trạng thái hiện tại của nhánh `main`

Tài liệu này được đồng bộ với code `main` ngày **06/09/2026**.

Nhánh `main` hiện đã có:

- OpenAI Chat Completions: `POST /v1/chat/completions`
- OpenAI Responses: `POST /v1/responses`
- Anthropic Messages: `POST /v1/messages`
- OpenAI-compatible model discovery: `GET /v1/models`
- persistent threads và `previous_response_id`
- SQLite conversation history, Structured Memory IR, compaction, semantic/hybrid retrieval
- multi-provider, multi-account routing và failover
- route affinity, task-aware routing, adaptive scoring, quota/cooldown
- browser account lifecycle và Chromium/CDP runtime
- browser streaming/cancellation
- provider-native conversation affinity cho các adapter đã hỗ trợ
- per-client API key, allowlist và request/token budget
- model groups với **strict ordered fallback tiers**
- enable/disable đồng bộ giữa account, model và group
- xóa account trực tiếp từ UI
- browserless/direct HTTP transport cho các provider đã hỗ trợ
- browser adapters tích hợp sẵn:
  - ChatGPT Web
  - Gemini Web
  - Qwen Web
  - DeepSeek Web
  - Xiaomi MiMo Studio Web
- UI local tích hợp sẵn, không cần build frontend riêng
- portable **Agent Skill** để Codex/ChatGPT/Claude-style agent dùng llmgateway như model runtime
- bộ smoke test, live acceptance runner, Linux/macOS/Windows CI

### Multimodal

Multimodal Gateway gồm file/image attachment, vision, voice và image generation hiện vẫn đang phát triển trên nhánh `feat/multimodal-gateway`.

**Không coi multimodal là tính năng của `main` cho tới khi nhánh đó được merge.**

Chi tiết trạng thái tổng thể: [docs/project-status.md](docs/project-status.md).

---

## Kiến trúc tổng quan

```text
Claude Code ─ Anthropic Messages ─┐
Codex ───── OpenAI Responses ─────┤
OpenCode ───── OpenAI Chat ───────┤
Local UI ───── Persistent Threads ┤
                                  ▼
                           ┌──────────────┐
                           │  llmgateway  │
                           └──────┬───────┘
                                  │
       ┌──────────────────────────┼──────────────────────────┐
       ▼                          ▼                          ▼
Conversation Engine        Model Catalog / Groups      Client Policies
       │                          │                          │
 transcript + memory       models/routes/accounts      keys/allowlists
 compaction/retrieval      priority/fallback tiers     budgets/policies
       └──────────────────────────┼──────────────────────────┘
                                  ▼
                            Route Planner
                                  │
          ┌───────────────┬───────┼─────────┬───────────────┐
          ▼               ▼       ▼         ▼               ▼
      ChatGPT Web      Gemini Web Qwen Web DeepSeek Web  MiMo Studio
          │               │       │         │               │
          └──────────── browser/CDP or browserless ─────────┘
                                  │
                                  └──── optional API routes
```

llmgateway là nơi sở hữu conversation state chuẩn. Provider chỉ là execution backend cho từng turn. Khi provider-native affinity được bật, gateway vẫn giữ transcript canonical trong SQLite.

---

## Yêu cầu môi trường

Tối thiểu:

- **Rust stable** và Cargo
- Git
- curl
- một trình duyệt Chromium-compatible nếu dùng browser account:
  - Google Chrome
  - Chromium
  - Microsoft Edge có thể dùng nếu cấu hình executable phù hợp

Khuyến nghị cho test/dev đầy đủ:

- Node.js 20+ để chạy fixture/UI checks
- Python 3 để chạy fake provider/stress tools
- Bash trên macOS/Linux hoặc Git Bash/WSL trên Windows
- Docker nếu muốn test image/container

Kiểm tra:

```bash
rustc --version
cargo --version
git --version
curl --version
node --version
python3 --version
docker --version
```

---

## Chạy nhanh local

### 1. Clone repository

```bash
git clone https://github.com/binhminhanh1235/llmgateway.git
cd llmgateway
git switch main
git pull
```

### 2. Tạo config và env local

```bash
cp config/llmgateway.example.toml config/llmgateway.toml
cp .env.example .env
```

Sửa `.env`:

```dotenv
LLMGATEWAY_API_KEY=thay_bang_key_local_cua_ban
OPENROUTER_API_KEY=
GEMINI_API_KEY_PRIMARY=
GEMINI_API_KEY_SECONDARY=
QWEN_API_KEY=
```

Nếu chỉ dùng browser account, các API key upstream có thể để trống. `LLMGATEWAY_API_KEY` vẫn nên được đặt để bảo vệ local API.

### 3. Build và chạy

Development:

```bash
cargo run
```

Release:

```bash
cargo run --release
```

Gateway mặc định:

```text
http://127.0.0.1:7331
```

UI:

```text
http://127.0.0.1:7331/
```

Health:

```bash
curl http://127.0.0.1:7331/_llmgateway/health
```

Database mặc định:

```text
data/llmgateway.db
```

### macOS: chạy một click

Repo có `start.command`:

```bash
chmod +x start.command
./start.command
```

Script sẽ:

1. giải phóng port 7331 nếu có process cũ;
2. dùng binary `target/release/llmgateway` nếu đã build;
3. nếu chưa có binary thì chạy `cargo run --release`;
4. chờ health endpoint sẵn sàng rồi mở UI.

> Lưu ý: script hiện dùng `kill -9` với process chiếm port 7331. Chỉ dùng khi bạn chắc port này dành cho llmgateway.

Hướng dẫn đầy đủ cho macOS, Linux, Windows, Docker, test và browser accounts: [docs/huong-dan-chay-local.md](docs/huong-dan-chay-local.md).

---

## Thêm browser account từ UI

Mở:

```text
http://127.0.0.1:7331/
```

Vào **Accounts** → **Add browser account**.

Managed presets hiện có:

| Preset | Provider kind | Login |
|---|---|---|
| ChatGPT | `browser-chatgpt` | chatgpt.com |
| Gemini | `browser-gemini` | gemini.google.com |
| Qwen | `browser-qwen` | chat.qwen.ai |
| DeepSeek | `browser-deepseek` | chat.deepseek.com |
| Xiaomi MiMo | `browser-mimo` | aistudio.xiaomimimo.com |

Flow chuẩn:

1. tạo browser account;
2. llmgateway tạo isolated Chromium profile;
3. bấm login/open browser;
4. đăng nhập provider bình thường;
5. tự xử lý CAPTCHA/2FA nếu provider yêu cầu;
6. verify session;
7. refresh model catalog;
8. enable model mong muốn;
9. thêm model vào group hoặc gọi trực tiếp.

Cookies/auth state không được trả ra qua API.

---

## Browserless transport

Một số provider có thể dùng session đã đăng nhập để gọi trực tiếp provider web backend mà không cần giữ Chromium chạy cho mỗi request.

Trong Accounts UI có thể bật/tắt transport policy khi adapter hỗ trợ.

Khái niệm chính:

- `browser-only`: dùng browser/CDP
- `browserless-preferred`: ưu tiên direct HTTP/browserless nếu session và adapter hỗ trợ
- nếu direct transport không còn hợp lệ, runtime có thể yêu cầu re-auth tùy provider

Browserless không có nghĩa là bỏ qua đăng nhập. User vẫn phải đăng nhập hợp lệ bằng browser trước để tạo auth state.

---

## Enable/disable và đồng bộ model

Trạng thái hiện tại được đồng bộ theo chuỗi:

```text
Account
  └─ Model của account
       └─ Catalog model
            └─ Model group eligibility
```

Khi disable account hoặc model:

- model vẫn có thể được giữ trong cấu hình group;
- model đó bị đánh dấu không eligible cho fallback;
- `GET /v1/models` chỉ expose model/group còn khả dụng;
- khi enable lại, group tự bỏ trạng thái ignore nếu model đã hợp lệ trở lại.

UI mặc định mở tab **Enabled**.

---

## Model groups và ordered fallback

Model group là virtual model có thể đại diện cho nhiều physical model.

Ví dụ mong muốn:

```text
Tier 10: GPT-5.6 Sol
   ↓ nếu không dùng được
Tier 20: Gemini Pro
   ↓
Tier 30: Qwen Coder
```

TOML:

```toml
[[virtual_models.llmgateway-coding.tiers]]
priority = 10
routes = ["chatgpt-sol"]

[[virtual_models.llmgateway-coding.tiers]]
priority = 20
routes = ["gemini-pro"]

[[virtual_models.llmgateway-coding.tiers]]
priority = 30
routes = ["qwen-coder"]
```

Tất cả route eligible ở tier nhỏ hơn phải được thử/hết khả năng trước khi chuyển sang tier tiếp theo. Adaptive scoring, quota, readiness và fairness vẫn xếp hạng bên trong cùng một tier.

Xem: [docs/model-groups.md](docs/model-groups.md).

---

## API tương thích

### OpenAI Chat Completions

```bash
curl -X POST http://127.0.0.1:7331/v1/chat/completions \
  -H "Authorization: Bearer $LLMGATEWAY_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "llmgateway-auto",
    "messages": [
      {"role": "user", "content": "Xin chào"}
    ]
  }'
```

Streaming:

```bash
curl -N -X POST http://127.0.0.1:7331/v1/chat/completions \
  -H "Authorization: Bearer $LLMGATEWAY_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "llmgateway-auto",
    "stream": true,
    "messages": [
      {"role": "user", "content": "Giải thích optimistic locking"}
    ]
  }'
```

### OpenAI Responses

```bash
curl -X POST http://127.0.0.1:7331/v1/responses \
  -H "Authorization: Bearer $LLMGATEWAY_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "llmgateway-auto",
    "input": "Viết một ví dụ Rust ngắn"
  }'
```

### Anthropic Messages

```bash
curl -X POST http://127.0.0.1:7331/v1/messages \
  -H "x-api-key: $LLMGATEWAY_API_KEY" \
  -H "anthropic-version: 2023-06-01" \
  -H "Content-Type: application/json" \
  -d '{
    "model": "llmgateway-coding",
    "max_tokens": 1024,
    "messages": [
      {"role": "user", "content": "Explain Java virtual threads"}
    ]
  }'
```

### Model discovery

```bash
curl http://127.0.0.1:7331/v1/models \
  -H "Authorization: Bearer $LLMGATEWAY_API_KEY"
```

---

## Persistent Threads

Tạo thread:

```bash
curl -X POST http://127.0.0.1:7331/v1/threads \
  -H "Authorization: Bearer $LLMGATEWAY_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{"title":"Demo","model":"llmgateway-auto"}'
```

Gửi turn mới:

```bash
curl -N -X POST http://127.0.0.1:7331/v1/threads/<thread_id>/messages \
  -H "Authorization: Bearer $LLMGATEWAY_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{"content":"Tiếp tục chủ đề trước","stream":true}'
```

Một số API thread:

```text
POST   /v1/threads
GET    /v1/threads
GET    /v1/threads/{thread_id}
DELETE /v1/threads/{thread_id}
POST   /v1/threads/{thread_id}/messages
GET    /v1/threads/{thread_id}/context
GET    /v1/threads/{thread_id}/memory
POST   /v1/threads/{thread_id}/compact
POST   /v1/threads/{thread_id}/retrieve
```

---

## Admin APIs quan trọng

```text
GET    /_llmgateway/health
GET    /_llmgateway/models
GET    /_llmgateway/accounts
GET    /_llmgateway/clients

GET    /_llmgateway/accounts/{account_id}/models
PATCH  /_llmgateway/accounts/{account_id}/models
POST   /_llmgateway/accounts/{account_id}/models/refresh

GET    /_llmgateway/browser-account-setup/providers
POST   /_llmgateway/browser-account-setup
PATCH  /_llmgateway/browser-account-setup/{account_id}

GET    /_llmgateway/browser-sessions
POST   /_llmgateway/browser-sessions/{session_id}/driver/launch
POST   /_llmgateway/browser-sessions/{session_id}/driver/verify
POST   /_llmgateway/browser-sessions/{session_id}/driver/stop
```

UI dùng thêm các admin endpoint cho account lifecycle, model/group state, tracing và diagnostics.

---

## Client policies và budgets

Có thể cấp key riêng cho Claude Code, Codex, OpenCode hoặc client khác:

```toml
[clients.codex]
key_env = "LLMGATEWAY_CODEX_KEY"
enabled = true
allowed_models = ["llmgateway-coding", "llmgateway-auto"]
execution_preference = "prefer-browser"
api_fallback = true
daily_request_limit = 2000
```

Client policy có thể giới hạn:

- model
- route
- browser/API transport
- API fallback
- daily/monthly request budget
- daily/monthly token budget

Xem: [docs/client-policies.md](docs/client-policies.md).

---

## AI Agent Skill ✅

Agent-Native P0 đã **shipped trên `main`** qua PR #91.

Bundle portable nằm tại:

```text
skills/llmgateway/
├── SKILL.md
├── references/
│   ├── api.md
│   ├── diagnostics.md
│   ├── operations.md
│   └── routing.md
├── scripts/llmgateway_agent.py
└── tests/test_llmgateway_agent.py
```

Mục tiêu là để Codex/ChatGPT/Claude-style agent dùng llmgateway như **model runtime**, còn Router của llmgateway tiếp tục là nơi duy nhất quyết định readiness, policy, quota, tier, health và fallback.

### Quick start cho agent

Gateway phải đang chạy trước:

```bash
cargo run --release
```

Thiết lập credential. Với normal inference nên dùng scoped client key; admin key chỉ cần cho diagnostics quản trị:

```bash
export LLMGATEWAY_CLIENT_API_KEY="client-key"
export LLMGATEWAY_API_KEY="admin-key"
export LLMGATEWAY_BASE_URL="http://127.0.0.1:7331"
```

Kiểm tra health và model mà client thực sự được phép dùng:

```bash
python3 skills/llmgateway/scripts/llmgateway_agent.py health
python3 skills/llmgateway/scripts/llmgateway_agent.py models
```

Gọi Responses hoặc Chat:

```bash
python3 skills/llmgateway/scripts/llmgateway_agent.py responses \
  llmgateway-auto "Giải thích kiến trúc hiện tại"

python3 skills/llmgateway/scripts/llmgateway_agent.py chat \
  llmgateway-coding "Review đoạn code này"
```

Anthropic Messages:

```bash
python3 skills/llmgateway/scripts/llmgateway_agent.py messages \
  llmgateway-coding "Giải thích Java CAS" --max-tokens 1024
```

Khi cần hiểu vì sao route được hoặc không được chọn:

```bash
python3 skills/llmgateway/scripts/llmgateway_agent.py explain \
  llmgateway-auto --prompt "debug Rust"
```

### Quy tắc chọn model

```text
GET /v1/models
      |
      v
ưu tiên logical model / model group
      |
      v
llmgateway Router
      |
      +-- client policy
      +-- readiness
      +-- ordered fallback tiers
      +-- quota / cooldown
      +-- task fit / health / fairness
      |
      v
physical provider route
```

Agent không nên hard-code provider/model chỉ vì route đó từng chạy được ở một phiên trước. Nếu client policy không expose model qua `/v1/models`, agent không được cố bypass bằng physical route ID.

### Permission boundary

| Mức | Ví dụ | Mặc định của helper |
|---|---|---|
| READ | health, models, accounts, groups, route explain, execution trace | Có |
| EXECUTE | Responses, Chat Completions, Anthropic Messages | Có |
| OPERATE | enable/disable, refresh models, restart browser runtime | Không |
| ADMIN | delete account, thay credential/policy, destructive config | Không |

Helper CLI cố ý chỉ expose **READ + EXECUTE**. Các mutation quản trị phải dùng API/UI tương ứng và cần đúng mức phê duyệt của người dùng.

### Dùng với Agent Skills-compatible client

Import/copy **toàn bộ folder `skills/llmgateway`**, không chỉ riêng `SKILL.md`, vì skill dùng progressive disclosure qua `references/` và helper trong `scripts/`.

Skill không tự quản credential, không lấy raw cookie/token, không bypass CAPTCHA/2FA/passkey và không tạo một routing engine thứ hai.

Chi tiết: [docs/agent-native-gateway.md](docs/agent-native-gateway.md).

---

## Test nhanh

Format/check:

```bash
cargo fmt --check
RUSTFLAGS="-D warnings" cargo check --all-targets
cargo clippy --all-targets
cargo test --all-targets
```

Smoke chính:

```bash
bash scripts/smoke-local.sh
bash scripts/smoke-openai-sdk.sh
bash scripts/smoke-model-groups.sh
bash scripts/smoke-browser-account-ux.sh
bash scripts/smoke-browser-provider.sh
bash scripts/smoke-browser-streaming.sh
```

Full CI parity được liệt kê trong [docs/huong-dan-chay-local.md](docs/huong-dan-chay-local.md).

Live browserless acceptance chỉ chạy khi đã có account thật đăng nhập:

```bash
scripts/live-browserless-acceptance.sh --account <account-id>
scripts/live-qwen-browserless-acceptance.sh --account <account-id>
scripts/live-mimo-browserless-acceptance.sh --account <account-id>
```

Không dùng mock/fake-CDP test làm bằng chứng live provider acceptance.

---

## Docker

Build:

```bash
docker build -t llmgateway:local .
```

Chạy container cần mount config/data và truyền env phù hợp. Với browser/CDP local, chạy native thường đơn giản hơn container vì Chromium profile và GUI login cần lifecycle riêng.

---

## Cấu trúc repository

```text
.
├── adapters/        # browser adapter JavaScript
├── config/          # config mẫu
├── docs/            # kiến trúc, routing, browser, memory, roadmap
├── examples/        # ví dụ
├── scripts/         # smoke/live/stress runners
├── skills/          # portable AI Agent Skills
├── src/             # Rust gateway
├── ui/              # local embedded UI
├── Cargo.toml
├── Dockerfile
├── start.command
└── README.md
```

---

## Tài liệu nên đọc

- [Trạng thái tổng thể main](docs/project-status.md)
- [Hướng dẫn chạy local đầy đủ](docs/huong-dan-chay-local.md)
- [Roadmap](docs/roadmap.md)
- [Browser Accounts UX](docs/browser-accounts-ux.md)
- [Browser Provider Adapters](docs/browser-provider-adapters.md)
- [Browser Streaming](docs/browser-streaming.md)
- [Browser-aware Routing](docs/browser-aware-routing.md)
- [Model Groups](docs/model-groups.md)
- [Client Policies](docs/client-policies.md)
- [Provider Conversation Affinity](docs/provider-conversation-affinity.md)
- [Semantic Retrieval](docs/semantic-retrieval.md)
- [Hybrid Retrieval](docs/hybrid-retrieval.md)
- [Memory Provenance](docs/memory-provenance.md)
- [Quota/Usage](docs/quota-usage.md)

---

## Security

llmgateway không được dùng để bypass cơ chế bảo vệ của provider.

Nguyên tắc:

- CAPTCHA/2FA/passkey do người dùng hoàn thành bình thường;
- không export raw cookies qua API;
- browser profile tách biệt theo session/account;
- CDP bind loopback;
- provider auth failure phải chuyển thành trạng thái cần user attention;
- tôn trọng quota, rate limit và điều khoản của provider;
- browserless chỉ dùng auth state hợp lệ đã được tạo qua login bình thường.

---

## License

MIT.
