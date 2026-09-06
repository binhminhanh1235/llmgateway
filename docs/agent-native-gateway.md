# Agent-Native llmgateway

Tracking lịch sử: issue #90 / PR #91.  
Runtime P1-P3: issue #92 / PR #93.  
Baseline main khi P1-P3 bắt đầu: `f85e54b8741a8a184bdc84b142c8b770adee29c0`.

## 1. Kiến trúc

Nguyên tắc trung tâm:

> Agent mô tả intent và capability requirements. llmgateway là nơi duy nhất quyết định route.

```text
Agent / MCP host
      |
      +-- llmgateway agent ...
      +-- POST /mcp
      +-- llmgateway mcp --stdio
      +-- OpenAI / Anthropic compatibility API
      |
      v
Agent Control / compatibility frontend
      |
      v
same llmgateway Router
      |
      +-- ClientPolicy
      +-- Model Catalog / Model Groups
      +-- readiness
      +-- ordered fallback tiers
      +-- quota / cooldown
      +-- execution preference
      +-- task-aware fit
      +-- adaptive health
      +-- fairness / recovery
      |
      v
provider route
```

Không có routing engine thứ hai trong Skill, CLI hoặc MCP.

## 2. Single-executable invariant

Production/runtime chỉ có một application executable:

```text
llmgateway          macOS/Linux
llmgateway.exe      Windows
```

Cùng binary cung cấp:

```bash
llmgateway
llmgateway agent ...
llmgateway mcp --stdio
```

Normal server cũng expose:

```text
POST /mcp
GET  /_llmgateway/agent/capabilities
POST /_llmgateway/agent/resolve
POST /_llmgateway/agent/diagnostics
```

Runtime không yêu cầu Python, pip, Node.js, npm hay MCP bridge riêng.

Python/Node/shell trong `scripts/` chỉ là source-repository tooling cho fake provider, fixtures, stress, smoke và live acceptance.

## 3. Agent Skill bundle

Sau single-binary migration, Skill chỉ còn instruction/reference:

```text
skills/llmgateway/
├── SKILL.md
└── references/
    ├── api.md
    ├── diagnostics.md
    ├── mcp.md
    ├── operations.md
    └── routing.md
```

Không có Python helper hoặc MCP implementation trong Skill bundle. Runtime behavior nằm trong Rust executable.

## 4. Credentials

Base URL mặc định:

```text
http://127.0.0.1:7331
```

Execution nên dùng scoped client key:

```bash
export LLMGATEWAY_CLIENT_API_KEY="client-key"
```

Admin diagnostics có thể dùng:

```bash
export LLMGATEWAY_API_KEY="admin-key"
```

Không log hoặc expose raw credential value cho agent.

## 5. Native Agent CLI

Gateway phải chạy trước khi CLI gọi local API:

```bash
llmgateway
```

Health và discovery:

```bash
llmgateway agent health
llmgateway agent models
llmgateway agent capabilities
```

Resolve requirements:

```bash
llmgateway agent resolve \
  --model llmgateway-auto \
  --task coding \
  --capability coding \
  --capability reasoning \
  --min-context-window 32000 \
  --prompt "implement retry"
```

Execute với cùng requirements:

```bash
llmgateway agent responses llmgateway-auto "implement retry" \
  --task coding \
  --capability coding \
  --capability reasoning \
  --min-context-window 32000

llmgateway agent chat llmgateway-auto "review code" \
  --capability coding

llmgateway agent messages llmgateway-coding "debug" \
  --max-tokens 1024 \
  --capability coding
```

Client-scoped diagnostics:

```bash
llmgateway agent diagnostics \
  --model llmgateway-auto \
  --capability coding
```

Non-mutating admin diagnostics vẫn có các command như `accounts`, `groups`, `clients`, `executions`, `explain`.

## 6. P1 - Agent Control API

P1 cung cấp:

```text
GET  /_llmgateway/agent/capabilities
POST /_llmgateway/agent/resolve
POST /_llmgateway/agent/diagnostics
```

Các endpoint dùng normal execution credential, vì vậy client policy của caller vẫn là hard boundary.

### Capability summary

`capabilities` tổng hợp:

- logical/virtual models;
- physical models;
- advertised capabilities;
- context window metadata;
- eligible route count;
- available account count.

### Resolve

Ví dụ:

```json
{
  "model": "llmgateway-auto",
  "task": "coding",
  "requirements": {
    "capabilities": ["coding", "reasoning"],
    "min_context_window": 32000
  }
}
```

`resolve` là dry-run Router, không gọi provider inference.

### Diagnostics

`diagnostics` dùng cùng evidence nhưng normalize blocking reasons và recommended action cho agent.

## 7. P2 - Native MCP

MCP không còn là Python bridge.

### HTTP

Normal server:

```bash
llmgateway
```

MCP endpoint:

```text
POST http://127.0.0.1:7331/mcp
```

Modern MCP path target protocol `2026-07-28` và dùng stateless request semantics.

### STDIO

Host chỉ hỗ trợ stdio:

```bash
llmgateway mcp --stdio
```

Đây vẫn là cùng Rust binary.

Tool surface:

- `llmgateway_health`;
- `llmgateway_capabilities`;
- `llmgateway_models`;
- `llmgateway_resolve`;
- `llmgateway_diagnostics`;
- `llmgateway_responses`;
- `llmgateway_chat`;
- `llmgateway_messages`.

Không expose OPERATE/ADMIN mutation tools.

## 8. P3 - Capability-based routing

Gateway-only extension:

```json
{
  "llmgateway_requirements": {
    "capabilities": ["coding"],
    "min_context_window": 32000
  }
}
```

Semantics:

- capability names normalize lowercase;
- underscore normalize thành hyphen;
- required capabilities là hard constraints;
- known context window nhỏ hơn minimum bị loại;
- unknown context metadata cũng bị loại khi minimum là hard requirement;
- client model/route policy không thể bị requirements mở rộng;
- model-group tiers và fallback order vẫn authoritative;
- readiness/quota/health/task fit/fairness vẫn chạy trong Router;
- Chat, Responses, Anthropic Messages, Agent CLI và MCP đều giữ cùng requirements;
- gateway-only fields bị strip trước upstream provider request.

Resolve và execution phải mang cùng requirements để tránh route drift.

## 9. Browser runtime

Agent/MCP không được tự chọn raw browser executable hoặc thao tác credential.

Browser executable là operator setting từ WebUI/API.

Accounts WebUI phát hiện browser CDP-compatible trên máy theo priority Auto:

1. Google Chrome;
2. Microsoft Edge;
3. Brave;
4. Chromium.

User có thể chọn browser cụ thể. Lựa chọn được persist và hot-reload cho lần browser launch tiếp theo, không cần restart gateway.

Browser auth vẫn user-owned:

- CAPTCHA/2FA/passkey interactive;
- cookies/local storage không expose qua Agent/MCP;
- browserless là transport optimization sau login hợp lệ, không phải auth bypass.

## 10. Permission boundary

### READ

- health;
- models/capabilities;
- route resolve/diagnostics;
- account/group/client inspection;
- execution trace.

### EXECUTE

- inference qua Responses/Chat/Messages.

### OPERATE

- enable/disable;
- refresh models;
- browser launch/restart/stop;
- browser runtime selection;
- quota reset.

### ADMIN

- delete;
- credential/policy change;
- destructive config/data mutation.

Native Agent/MCP mặc định chỉ expose READ + EXECUTE và non-mutating diagnostics.

## 11. Verification

Dedicated runtime smoke:

```bash
bash scripts/smoke-agent-control.sh
bash scripts/smoke-native-agent-mcp.sh
```

`smoke-native-agent-mcp.sh` kiểm tra:

- native `llmgateway agent`;
- native MCP HTTP;
- native MCP stdio;
- semantic resolve;
- execution through same requirements;
- browser discovery;
- Google Chrome auto-detection priority;
- browser selection persistence/hot reload.

Rust gates:

```bash
RUSTFLAGS="-D warnings" cargo check --all-targets
cargo clippy --all-targets
cargo test --all-targets
```

P1-P3 + single-executable migration chỉ được gọi DONE / VERIFIED khi exact-head CI trên final tree xanh Linux, Windows, existing smoke suite và Docker.

## 12. Current branch state

PR #93 đang mở và **chưa merge vào main**.

Implementation P1-P3 cũ đã từng xanh tại `e05f32b25f24b9886bd45e7876f544afa86cd83b` / CI #1798. Sau đó kiến trúc P2 được nâng thành native single-binary và browser runtime selector được thêm theo yêu cầu mới, vì vậy evidence cũ không được dùng làm final gate cho tree hiện tại.

## 13. Non-goals

- credential extraction;
- CAPTCHA/2FA/passkey bypass;
- autonomous unrestricted admin;
- provider ranking hard-coded trong Skill;
- second routing engine;
- Python/Node runtime dependency;
- separate MCP application;
- browser credential exposure;
- gọi feature branch là shipped trước merge/post-merge verification.
