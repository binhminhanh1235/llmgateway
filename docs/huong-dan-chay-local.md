# Hướng dẫn chạy llmgateway local đầy đủ

Cập nhật: **06/09/2026**

Tài liệu này dành cho nhánh `main`.

## 1. Mục tiêu

Sau khi hoàn tất tài liệu này, bạn có thể:

- chạy llmgateway native trên macOS/Linux/Windows;
- mở local UI;
- thêm browser account;
- chạy qua browser hoặc browserless khi provider hỗ trợ;
- gọi OpenAI/Anthropic-compatible API;
- tạo model group/fallback;
- chạy test gần tương đương CI;
- build Docker image;
- xử lý các lỗi local thường gặp.

## 2. Prerequisites

### Bắt buộc

- Git
- Rust stable + Cargo
- curl

### Khi dùng browser accounts

- Google Chrome hoặc Chromium-compatible browser
- GUI session để đăng nhập provider lần đầu

### Khi chạy test đầy đủ

- Node.js 20+
- Python 3
- Bash
- Docker

Windows có thể dùng:

- PowerShell cho native Rust
- Git Bash hoặc WSL cho các `.sh` smoke tests

## 3. Clone và checkout main

```bash
git clone https://github.com/binhminhanh1235/llmgateway.git
cd llmgateway
git switch main
git pull --ff-only
```

Kiểm tra:

```bash
git status
git log -1 --oneline
```

## 4. Tạo config local

```bash
cp config/llmgateway.example.toml config/llmgateway.toml
cp .env.example .env
```

Windows PowerShell:

```powershell
Copy-Item config/llmgateway.example.toml config/llmgateway.toml
Copy-Item .env.example .env
```

## 5. Cấu hình API key local

Mở `.env`:

```dotenv
LLMGATEWAY_API_KEY=local_secret_change_me
OPENROUTER_API_KEY=
GEMINI_API_KEY_PRIMARY=
GEMINI_API_KEY_SECONDARY=
QWEN_API_KEY=
```

Nếu chỉ dùng browser account:

- giữ `LLMGATEWAY_API_KEY`;
- upstream API keys có thể để trống.

Nếu dùng API provider:

- điền đúng key tương ứng;
- giữ account/provider trong TOML enabled.

Không commit `.env` hoặc config chứa secret thật.

## 6. Chạy native

### Development

```bash
cargo run
```

### Release

```bash
cargo run --release
```

### Build trước rồi chạy

```bash
cargo build --release
./target/release/llmgateway
```

Windows:

```powershell
cargo build --release
.\target\release\llmgateway.exe
```

## 7. macOS one-click start

```bash
chmod +x start.command
./start.command
```

Script sẽ:

- tìm process chiếm port 7331;
- stop process đó;
- chạy release binary nếu đã có;
- fallback sang `cargo run --release`;
- chờ health OK;
- tự mở UI.

Nếu không muốn script force-kill process trên port 7331, chạy `cargo run --release` thủ công.

## 8. Kiểm tra gateway

Health:

```bash
curl http://127.0.0.1:7331/_llmgateway/health
```

UI:

```text
http://127.0.0.1:7331/
```

Model list:

```bash
curl http://127.0.0.1:7331/v1/models \
  -H "Authorization: Bearer $LLMGATEWAY_API_KEY"
```

Nếu shell chưa export key:

macOS/Linux:

```bash
export LLMGATEWAY_API_KEY=local_secret_change_me
```

PowerShell:

```powershell
$env:LLMGATEWAY_API_KEY="local_secret_change_me"
```

## 9. Thêm browser account

Mở UI → **Accounts** → **Add browser account**.

Managed presets:

- ChatGPT
- Gemini
- DeepSeek
- Xiaomi MiMo
- Qwen

Flow:

1. chọn provider;
2. đặt account ID/label nếu cần;
3. tạo account;
4. bấm Login/Open browser;
5. đăng nhập provider;
6. hoàn thành CAPTCHA/2FA/passkey thủ công;
7. verify session;
8. refresh model;
9. bật model muốn dùng;
10. test chat.

Browser profile được tách biệt theo session/account.

## 10. Nếu Chromium không tự tìm thấy

Trong `config/llmgateway.toml`:

```toml
[chromium]
enabled = true
executable = "/duong/dan/toi/chrome"
startup_timeout_seconds = 15
auto_recover = true
reconcile_interval_seconds = 15
extra_args = []
```

macOS thường gặp:

```text
/Applications/Google Chrome.app/Contents/MacOS/Google Chrome
```

Linux thường gặp:

```text
/usr/bin/google-chrome
/usr/bin/chromium
/usr/bin/chromium-browser
```

Windows ví dụ:

```toml
executable = "C:\\Program Files\\Google\\Chrome\\Application\\chrome.exe"
```

Sau khi sửa config, restart gateway nếu thay đổi đó chưa được hot-reload bởi flow UI tương ứng.

## 11. Browserless

Browserless chỉ khả dụng khi adapter/provider công bố support.

Điều kiện cơ bản:

- account đã login hợp lệ bằng browser;
- auth snapshot còn hợp lệ;
- provider direct adapter healthy;
- transport policy bật browserless.

UI Accounts có toggle khi supported.

Để test, stop Chromium rồi gửi request. Nếu direct transport hoạt động, request vẫn thành công mà browser process không cần chạy.

Live runner tổng quát:

```bash
scripts/live-browserless-acceptance.sh --account <account-id>
```

Qwen:

```bash
scripts/live-qwen-browserless-acceptance.sh --account <account-id>
```

MiMo:

```bash
scripts/live-mimo-browserless-acceptance.sh --account <account-id>
```

Gemini model selection:

```bash
scripts/live-gemini-model-selection.sh --account <account-id>
```

PowerShell equivalents có trong `scripts/` cho các runner được hỗ trợ.

## 12. Test Chat Completions

```bash
curl -X POST http://127.0.0.1:7331/v1/chat/completions \
  -H "Authorization: Bearer $LLMGATEWAY_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "model":"llmgateway-auto",
    "messages":[
      {"role":"user","content":"hi"}
    ]
  }'
```

Streaming:

```bash
curl -N -X POST http://127.0.0.1:7331/v1/chat/completions \
  -H "Authorization: Bearer $LLMGATEWAY_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "model":"llmgateway-auto",
    "stream":true,
    "messages":[
      {"role":"user","content":"Explain zero-copy in Kafka"}
    ]
  }'
```

## 13. Test Responses API

```bash
curl -X POST http://127.0.0.1:7331/v1/responses \
  -H "Authorization: Bearer $LLMGATEWAY_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "model":"llmgateway-auto",
    "input":"Give me three Rust ownership rules"
  }'
```

## 14. Test Anthropic Messages

```bash
curl -X POST http://127.0.0.1:7331/v1/messages \
  -H "x-api-key: $LLMGATEWAY_API_KEY" \
  -H "anthropic-version: 2023-06-01" \
  -H "Content-Type: application/json" \
  -d '{
    "model":"llmgateway-coding",
    "max_tokens":512,
    "messages":[
      {"role":"user","content":"Explain Java CAS"}
    ]
  }'
```

## 15. Persistent thread smoke

Tạo:

```bash
curl -X POST http://127.0.0.1:7331/v1/threads \
  -H "Authorization: Bearer $LLMGATEWAY_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{"title":"local-smoke","model":"llmgateway-auto"}'
```

Lấy `thread_id`, rồi:

```bash
curl -N -X POST http://127.0.0.1:7331/v1/threads/<thread_id>/messages \
  -H "Authorization: Bearer $LLMGATEWAY_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{"content":"Remember code name ORBIT-42","stream":true}'
```

Turn tiếp:

```bash
curl -N -X POST http://127.0.0.1:7331/v1/threads/<thread_id>/messages \
  -H "Authorization: Bearer $LLMGATEWAY_API_KEY" \
  -H "Content-Type: application/json" \
  -d '{"content":"What code name did I give you?","stream":true}'
```

## 16. Model groups từ UI

Vào **Model Groups**.

Quy tắc:

- chỉ model fallback-eligible được chọn mới;
- model đã nằm trong group vẫn có thể được giữ khi account/model disable;
- disabled item bị ignored khi routing;
- enable lại sẽ phục hồi eligibility;
- tier số nhỏ hơn có ưu tiên cao hơn.

Ví dụ:

```text
10 GPT-5.6 Sol
20 Gemini Pro
30 Qwen Coder
```

Test smoke:

```bash
bash scripts/smoke-model-groups.sh
```

## 17. Kiểm tra account/model state sync

Sau khi disable account:

1. Accounts tab phải chuyển account sang Disabled;
2. model tương ứng không được xem là fallback eligible;
3. Models tab không được hiển thị nó như active;
4. model group vẫn có thể giữ membership nhưng ignored;
5. `GET /v1/models` không được expose route/group không còn viable.

Enable lại rồi xác minh ngược lại.

Automated test:

```bash
node scripts/test-status-sync-ui.mjs
bash scripts/smoke-model-groups.sh
```

## 17.1 Dùng AI Agent Skill

Agent-Native P0 đã có trên `main`.

Bundle:

```text
skills/llmgateway/
├── SKILL.md
├── references/
├── scripts/llmgateway_agent.py
└── tests/test_llmgateway_agent.py
```

Thiết lập env:

```bash
export LLMGATEWAY_CLIENT_API_KEY="client-key"
export LLMGATEWAY_API_KEY="admin-key"
export LLMGATEWAY_BASE_URL="http://127.0.0.1:7331"
```

Smoke nhanh:

```bash
python3 skills/llmgateway/scripts/llmgateway_agent.py health
python3 skills/llmgateway/scripts/llmgateway_agent.py models
python3 skills/llmgateway/scripts/llmgateway_agent.py responses llmgateway-auto "hello"
python3 skills/llmgateway/scripts/llmgateway_agent.py explain llmgateway-auto --prompt "coding task"
```

Test helper/bundle:

```bash
python3 -m unittest skills/llmgateway/tests/test_llmgateway_agent.py
```

Helper chỉ expose READ + EXECUTE. Enable/disable/delete và các mutation quản trị vẫn phải đi qua UI/admin API với đúng authorization.

Nếu dùng Agent Skills-compatible client, copy/import toàn bộ folder `skills/llmgateway`, không chỉ riêng `SKILL.md`.

Xem thêm: [agent-native-gateway.md](agent-native-gateway.md).

## 18. Basic Rust quality gate

```bash
cargo fmt --check
RUSTFLAGS="-D warnings" cargo check --all-targets
cargo clippy --all-targets
cargo test --all-targets
```

## 19. UI/adapter syntax checks

```bash
node --check ui/app.js
node scripts/test-status-sync-ui.mjs
node scripts/test-account-transport-ui.mjs
node --check ui/account-control.js
node --check ui/account-intelligence.js
node --check ui/browser-control.js
node --check ui/model-groups.js
node --check ui/trace-console.js

node --check adapters/gemini-web.js
node --check adapters/chatgpt-web.js
node --check adapters/qwen-web.js
node --check adapters/deepseek-web.js
node --check adapters/mimo-web.js

node scripts/test-browser-adapter-fixtures.mjs
python3 -m unittest skills/llmgateway/tests/test_llmgateway_agent.py
```

## 20. Shell syntax checks

```bash
bash -n scripts/live-browserless-acceptance.sh
bash -n scripts/live-gemini-model-selection.sh
bash -n scripts/live-qwen-browserless-acceptance.sh
bash -n scripts/live-mimo-browserless-acceptance.sh
bash -n scripts/smoke-gemini-recovery.sh
```

## 21. Full deterministic smoke suite gần CI

Chạy tuần tự:

```bash
bash scripts/smoke-provider-conversation-affinity.sh
bash scripts/smoke-gemini-recovery.sh
bash scripts/smoke-chatgpt-browser.sh
bash scripts/smoke-local.sh
bash scripts/smoke-openai-sdk.sh
bash scripts/smoke-hybrid.sh
bash scripts/smoke-memory.sh
bash scripts/smoke-quota.sh
bash scripts/smoke-browser-session.sh
bash scripts/smoke-chromium-driver.sh
bash scripts/smoke-browser-account-ux.sh
bash scripts/smoke-browser-streaming.sh
bash scripts/smoke-browser-provider.sh
bash scripts/smoke-first-class-browser-account.sh
bash scripts/smoke-browser-reliability.sh
bash scripts/smoke-browser-routing-intelligence.sh
bash scripts/smoke-account-intelligence.sh
bash scripts/smoke-routing-trace.sh
bash scripts/smoke-model-groups.sh
bash scripts/smoke-execution-trace.sh
bash scripts/smoke-adaptive-routing.sh
bash scripts/smoke-task-aware-routing.sh
bash scripts/smoke-client-policies.sh
```

Các smoke này phần lớn dùng fake provider/CDP deterministic. Chúng không chứng minh provider website thật đang hoạt động.

## 22. Windows test

CI Windows chạy:

```powershell
cargo check --all-targets
cargo test --all-targets
powershell -NoProfile -ExecutionPolicy Bypass -File scripts/smoke-chromium-driver-windows.ps1
```

PowerShell parser cũng validate các live runner `.ps1`.

## 23. Docker build

```bash
docker build -t llmgateway:local .
```

Smoke giống CI:

```bash
docker build -t llmgateway:ci .
```

Native browser-account workflow phù hợp hơn container nếu bạn cần:

- visible Chromium window;
- interactive login;
- persistent local browser profiles.

## 24. Data cần backup

Quan trọng:

```text
data/llmgateway.db
data/browser-profiles/
data/browser-auth/
config/llmgateway.toml
.env
```

Không đưa browser-auth/profile hoặc `.env` vào Git.

## 25. Reset local sạch

Dừng gateway trước.

Backup nếu cần:

```bash
cp -R data data.backup
```

Để reset database:

```bash
rm -f data/llmgateway.db
```

Không xóa `data/browser-profiles` nếu muốn giữ login session.

Chỉ xóa browser profile khi bạn thực sự muốn re-login từ đầu.

## 26. Port 7331 bị chiếm

macOS/Linux:

```bash
lsof -i :7331
```

Stop nhẹ trước:

```bash
kill <pid>
```

Chỉ dùng `kill -9` khi process không chịu dừng.

Windows:

```powershell
Get-NetTCPConnection -LocalPort 7331
Stop-Process -Id <pid>
```

## 27. Browser transport failed

Kiểm tra theo thứ tự:

1. account enabled?
2. model enabled?
3. browser session state?
4. Chromium process đang chạy?
5. CDP reachable?
6. login còn hiệu lực?
7. adapter probe healthy?
8. model picker/model binding hợp lệ?
9. provider có challenge/CAPTCHA?
10. browserless auth snapshot có stale?
11. route/group có đang ignore model?

Không reset profile ngay từ đầu. Ưu tiên Verify/Re-authenticate/Restart.

## 28. Model không xuất hiện

Thử:

1. Accounts → refresh models;
2. kiểm tra account enabled;
3. kiểm tra model toggle;
4. xem Models tab Enabled/Disabled;
5. kiểm tra group viability;
6. gọi `GET /v1/models`;
7. xem Trace/diagnostic;
8. kiểm tra provider model discovery.

Model discovery có thể khác giữa các account vì provider rollout/quota/account entitlement.

## 29. Browserless không hoạt động

Kiểm tra:

- provider có support direct transport không;
- toggle đã bật không;
- account đã login qua browser trước chưa;
- auth snapshot có được capture không;
- session đã hết hạn chưa;
- provider backend có đổi protocol không.

Nếu direct adapter báo auth invalid, mở browser và re-authenticate thay vì cố retry vô hạn.

## 30. Live acceptance vs smoke test

Phân biệt:

### Deterministic smoke

- fake provider
- fake CDP
- CI-friendly
- chứng minh gateway contract

### Live acceptance

- account thật
- website/provider backend thật
- auth thật
- chứng minh integration thực tế

Một feature phụ thuộc web provider chỉ được gọi là live-verified khi runner thật pass với account thật.

## 31. Multimodal local test

Multimodal hiện không thuộc main. Nếu bạn checkout `feat/multimodal-gateway`, hãy dùng tài liệu/test runner riêng trên nhánh đó.

Không áp dụng multimodal API/file/media commands vào `main` hiện tại và kỳ vọng chúng tồn tại.

## 32. Checklist trước khi báo bug

Thu thập:

- exact commit: `git rev-parse HEAD`
- OS
- Rust version
- provider/account kind
- browser-only hay browserless
- model ID
- API surface: Chat/Responses/Messages/Threads
- streaming hay non-streaming
- error text đầy đủ
- Trace Console entry
- browser lifecycle state
- adapter state
- steps reproduce

Không gửi raw cookie, token hoặc API key vào issue/log công khai.
