# Dùng llmgateway với Hermes Agent

Tài liệu này áp dụng cho kiến trúc **single executable** trên `feat/agent-native-runtime` / PR #93.

## Không cần uv, Python, bun hay npm

Production runtime chỉ cần binary:

```text
llmgateway
```

Agent CLI và MCP đều nằm trong cùng executable:

```bash
llmgateway agent ...
llmgateway mcp --stdio
```

Normal server cũng expose MCP HTTP tại:

```text
POST http://127.0.0.1:7331/mcp
```

Các lệnh sau là **không đúng** với runtime hiện tại và không nên dùng:

```bash
uv run mcp-server
bun run start:mcp
uv run pytest tests/test_agent.py
uv run python -m llmgateway.agent
```

Repo có thể còn Python/Node trong `scripts/` cho fake provider, fixtures, stress và CI, nhưng llmgateway production không spawn hoặc yêu cầu chúng.

## 1. Build binary nếu chạy từ source

Từ repo:

```bash
cd /Users/thando/Documents/llmgateway
cargo build --release
```

Binary:

```text
/Users/thando/Documents/llmgateway/target/release/llmgateway
```

Nếu dùng release artifact đã tải sẵn thì bỏ qua bước build.

## 2. Start llmgateway

```bash
cd /Users/thando/Documents/llmgateway
./target/release/llmgateway
```

Mặc định:

```text
WebUI:  http://127.0.0.1:7331/
MCP:    http://127.0.0.1:7331/mcp
API:    http://127.0.0.1:7331/v1/...
```

## 3. Cài Agent Skill cho Hermes

Skill chỉ là instruction/reference, không phải Python package.

Copy:

```bash
mkdir -p ~/.hermes/skills
cp -R /Users/thando/Documents/llmgateway/skills/llmgateway ~/.hermes/skills/
```

Hoặc symlink để thay đổi trong repo được phản ánh ngay:

```bash
mkdir -p ~/.hermes/skills
rm -rf ~/.hermes/skills/llmgateway
ln -s /Users/thando/Documents/llmgateway/skills/llmgateway ~/.hermes/skills/llmgateway
```

Sau đó reload Hermes.

Nếu bản Hermes của bạn có command liệt kê skill:

```bash
hermes skill list
```

Nếu CLI của Hermes dùng tên command khác, chỉ cần xác nhận folder `llmgateway` đã nằm trong skill directory mà Hermes đang đọc.

## 4. Cấu hình MCP cho Hermes bằng cùng binary

Nếu Hermes dùng MCP stdio command configuration:

```yaml
mcp_servers:
  llmgateway:
    command: "/Users/thando/Documents/llmgateway/target/release/llmgateway"
    args:
      - "mcp"
      - "--stdio"
    env:
      LLMGATEWAY_BASE_URL: "http://127.0.0.1:7331"
      LLMGATEWAY_CLIENT_API_KEY: "YOUR_CLIENT_KEY"
```

Không dùng:

```yaml
command: "uv"
args: ["run", "mcp-server"]
```

Hermes sẽ spawn **cùng executable llmgateway** ở stdio mode. Gateway chính vẫn phải đang chạy vì stdio frontend dùng cùng local Agent Control/API contracts và cùng Router.

Nếu Hermes hỗ trợ MCP HTTP hiện đại, ưu tiên trỏ trực tiếp tới:

```text
http://127.0.0.1:7331/mcp
```

với normal llmgateway client credential. Khi đó không cần spawn thêm process stdio.

## 5. Test native Agent CLI

Thiết lập credential:

```bash
export LLMGATEWAY_BASE_URL="http://127.0.0.1:7331"
export LLMGATEWAY_CLIENT_API_KEY="YOUR_CLIENT_KEY"
```

Health:

```bash
/Users/thando/Documents/llmgateway/target/release/llmgateway agent health
```

Models:

```bash
/Users/thando/Documents/llmgateway/target/release/llmgateway agent models
```

Capabilities:

```bash
/Users/thando/Documents/llmgateway/target/release/llmgateway agent capabilities
```

Capability-aware resolve:

```bash
/Users/thando/Documents/llmgateway/target/release/llmgateway agent resolve \
  --model llmgateway-auto \
  --capability coding \
  --prompt "Ping test"
```

Execute:

```bash
/Users/thando/Documents/llmgateway/target/release/llmgateway agent chat \
  llmgateway-auto "Ping test" \
  --capability coding
```

Không cần:

```bash
uv run pytest ...
uv run python -m llmgateway.agent ...
```

## 6. Test MCP stdio trực tiếp

Discovery:

```bash
printf '%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"server/discover","params":{"_meta":{"io.modelcontextprotocol/protocolVersion":"2026-07-28"}}}' \
  | /Users/thando/Documents/llmgateway/target/release/llmgateway mcp --stdio
```

Expected response phải chứa:

```text
resultType
supportedVersions
llmgateway
```

## 7. Browser runtime

Browser cũng không cần cài qua uv/npm.

Vào:

```text
WebUI → Accounts → Browser runtime
```

llmgateway tự quét browser compatible trên máy và cho chọn:

1. Google Chrome, Auto ưu tiên;
2. Microsoft Edge;
3. Brave;
4. Chromium.

User chọn browser trực tiếp trên WebUI. Lựa chọn được persist và áp dụng từ lần browser launch tiếp theo.

## 8. Mô hình runtime cuối cùng

```text
Hermes Skill
    |
    +-- instructions only
    |
    v
Hermes
    |
    +-- MCP HTTP ----------> llmgateway :7331/mcp
    |
    +-- hoặc spawn stdio --> llmgateway mcp --stdio
                              |
                              v
                        same llmgateway
                        Router / Catalog
                        ClientPolicy
                        Model Groups
                        Providers
```

Về phía user, application duy nhất cần cài/chạy là **llmgateway**. Browser là browser engine có sẵn trên máy nếu dùng browser accounts.
