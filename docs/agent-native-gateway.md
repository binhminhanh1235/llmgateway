# Agent-Native llmgateway

Tracking: issue #90.  
Shipped: PR #91.  
Main merge commit: `777c7cf3fa8b9d25a4d49ea46c2e5822e88548d3`.

## 1. Trạng thái

P0 - Portable Agent Skill đã **DONE / VERIFIED / SHIPPED trên `main`**.

P0 cung cấp:

- `skills/llmgateway/SKILL.md`;
- progressive-disclosure references cho API, routing, diagnostics và operations;
- helper CLI Python stdlib-only;
- model discovery theo client policy;
- logical model/model-group first;
- READ / EXECUTE / OPERATE / ADMIN permission boundary;
- deterministic offline tests được nối vào CI.

Exact-head PR CI #1760 đã pass Linux, Windows, Rust checks/tests, smoke suites và Docker build trước khi merge.

## 2. Mục tiêu kiến trúc

Agent nói **intent**. llmgateway quyết định **eligible route**.

```text
Agent intent
    |
    v
llmgateway Agent Skill
    |
    +-- discover client-visible models
    +-- choose logical model/group
    +-- call compatible API
    +-- inspect diagnostics when needed
    |
    v
llmgateway Router
    |
    +-- client policy
    +-- readiness
    +-- ordered fallback tiers
    +-- quota/cooldown
    +-- task-aware routing
    +-- adaptive health
    +-- browser/API policy
    +-- fairness/recovery
    |
    v
provider route
```

Skill phải mỏng. Nếu skill tự chứa bảng "provider X tốt hơn provider Y" hoặc tự implement fallback, hệ thống sẽ có hai routing engines và sớm bị drift.

## 3. Bundle layout

```text
skills/llmgateway/
├── SKILL.md
├── references/
│   ├── api.md
│   ├── diagnostics.md
│   ├── operations.md
│   └── routing.md
├── scripts/
│   └── llmgateway_agent.py
└── tests/
    └── test_llmgateway_agent.py
```

`SKILL.md` giữ workflow ngắn. Nội dung chi tiết được tách sang `references/` để agent chỉ đọc khi cần.

## 4. Kết nối

Base URL mặc định:

```text
http://127.0.0.1:7331
```

Có thể override bằng:

```bash
export LLMGATEWAY_BASE_URL="http://127.0.0.1:7331"
```

Normal inference nên dùng scoped client key:

```bash
export LLMGATEWAY_CLIENT_API_KEY="client-key"
```

Admin diagnostics dùng:

```bash
export LLMGATEWAY_API_KEY="admin-key"
```

Không log hoặc trả credential value cho agent output.

## 5. Workflow chuẩn

### Bước 1 - Health

```bash
python3 skills/llmgateway/scripts/llmgateway_agent.py health
```

Nếu gateway không reachable, dừng ở lỗi process/network. Không đổ lỗi provider trước khi gateway sống.

### Bước 2 - Discover model

```bash
python3 skills/llmgateway/scripts/llmgateway_agent.py models
```

Kết quả `/v1/models` là model/group mà credential hiện tại thực sự được phép request.

### Bước 3 - Chọn logical model/group

Nếu user không yêu cầu exact provider/model, ưu tiên `llmgateway-*` logical model/group phù hợp rồi để Router xử lý eligibility và fallback.

Không bypass client policy bằng physical route ID.

### Bước 4 - Execute

Responses:

```bash
python3 skills/llmgateway/scripts/llmgateway_agent.py responses \
  llmgateway-auto "Tóm tắt task"
```

Chat Completions:

```bash
python3 skills/llmgateway/scripts/llmgateway_agent.py chat \
  llmgateway-coding "Review code"
```

Anthropic Messages:

```bash
python3 skills/llmgateway/scripts/llmgateway_agent.py messages \
  llmgateway-coding "Debug lỗi" --max-tokens 1024
```

### Bước 5 - Diagnose khi cần

```bash
python3 skills/llmgateway/scripts/llmgateway_agent.py explain \
  llmgateway-auto --prompt "debug Rust"

python3 skills/llmgateway/scripts/llmgateway_agent.py accounts
python3 skills/llmgateway/scripts/llmgateway_agent.py groups
python3 skills/llmgateway/scripts/llmgateway_agent.py executions
```

Các admin diagnostics cần `LLMGATEWAY_API_KEY`.

## 6. Permission model

### READ

Cho phép inspection không mutation:

- health;
- model discovery;
- accounts/groups/clients;
- route explain;
- execution trace;
- browser/runtime diagnostics.

### EXECUTE

Cho phép inference qua compatibility APIs. Hoạt động này có thể tiêu quota/budget.

### OPERATE

Bao gồm:

- enable/disable account/model/group;
- refresh model discovery;
- restart/stop browser runtime;
- đổi browserless transport preference;
- reset quota state.

Chỉ thực hiện khi user rõ ràng yêu cầu state change.

### ADMIN

Bao gồm:

- delete account;
- xóa group;
- đổi secret/credential;
- mở rộng client authorization policy;
- destructive config/data mutation.

Các hành động này cần explicit approval cho đúng action.

Helper bundled cố ý không expose OPERATE/ADMIN command.

## 7. Diagnostic decision tree

Khi request lỗi:

1. kiểm tra gateway health;
2. gọi `/v1/models` bằng đúng execution credential;
3. route explain cho logical model;
4. kiểm tra account/model/group state;
5. xem execution trace;
6. nếu browser-backed, kiểm tra runtime/session/adapter/auth;
7. retry chỉ khi failure được classify là retryable.

Phân biệt rõ:

- gateway unavailable;
- client policy exclusion;
- account/model disabled;
- provider model discovery drift;
- auth expired;
- page/adapter drift;
- CDP/browser transport failure;
- browserless/direct transport failure;
- quota/rate limit;
- provider-native model binding conflict.

Deletion không phải diagnostic step.

## 8. Browser/provider safety

- browser auth thuộc user;
- CAPTCHA/2FA/passkey luôn interactive;
- không export cookies, local-storage token hoặc refresh token;
- browserless là transport optimization sau authenticated session, không phải auth bypass;
- không retry vô hạn khi provider quota/challenge đang chặn.

## 9. Multimodal

Agent Skill chỉ feature-detect capability có trên **current main**.

Không được suy luận rằng file/vision/voice/image-generation API đã ship chỉ vì provider hoặc branch khác hỗ trợ. Multimodal initiative vẫn phải theo source of truth riêng cho tới khi merge vào `main`.

## 10. Test

Chạy riêng skill tests:

```bash
python3 -m unittest skills/llmgateway/tests/test_llmgateway_agent.py
```

Tests hiện kiểm tra:

- execution auth header;
- Anthropic auth/version headers;
- admin route-explain request;
- helper không có destructive command;
- skill metadata và reference bundle tồn tại.

Full CI vẫn chạy cùng Rust, UI/adapter checks, smoke suites, Windows và Docker.

## 11. Cách import skill

Với client hỗ trợ Agent Skills, import/copy **cả folder `skills/llmgateway`** vào skill directory/workspace của client.

Không copy riêng `SKILL.md`, vì các links tương đối tới `references/` và helper scripts là một phần của bundle contract.

Nếu client không hỗ trợ Agent Skills native, vẫn có thể dùng helper CLI trực tiếp hoặc gọi compatibility APIs của llmgateway.

## 12. Future slices

### P1 - Agent Control API

Planned.

Chỉ thêm compact agent-facing status/capability views khi admin API hiện tại quá verbose. Reuse Router và state stores hiện có.

Candidate:

- capability summary;
- normalized diagnostic snapshot;
- safe probe endpoint.

### P2 - MCP server

Planned.

Expose selected llmgateway operations thành narrow MCP tools, tách read/execute/mutation permission.

### P3 - Capability-based Agent Routing

Planned.

Cho phép agent biểu đạt requirement như coding, reasoning, vision hoặc context mà không phụ thuộc provider brand. Implementation phải mở rộng Model Catalog/Router hiện tại, không bypass model groups/client policies.

## 13. Non-goals

- automated credential extraction;
- CAPTCHA/2FA bypass;
- unrestricted autonomous admin;
- provider-specific model ranking hard-coded trong skill;
- routing engine thứ hai ở agent layer;
- tuyên bố unmerged capability là shipped.
