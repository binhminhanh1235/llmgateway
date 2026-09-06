#!/bin/bash
# Move to the directory containing this script
cd "$(dirname "$0")"

echo "=========================================="
echo "         Khởi động LLM Gateway            "
echo "=========================================="

# Giải phóng port 7331 nếu đang có tiến trình cũ chiếm dụng
OLD_PID=$(lsof -t -i :7331 2>/dev/null)
if [ -n "$OLD_PID" ]; then
    echo "Phát hiện tiến trình cũ (PID: $OLD_PID) đang dùng port 7331. Đang dừng lại..."
    kill -9 $OLD_PID 2>/dev/null
    sleep 0.5
fi

# Tự động mở trình duyệt web khi gateway sẵn sàng
(
    while ! curl -s http://127.0.0.1:7331/_llmgateway/health >/dev/null 2>&1; do
        sleep 0.3
    done
    echo "✓ LLM Gateway đã sẵn sàng! Đang mở trình duyệt..."
    open "http://127.0.0.1:7331/"
) &

# Chạy binary release
if [ -f "./target/release/llmgateway" ]; then
    echo "Đang khởi chạy ./target/release/llmgateway..."
    exec ./target/release/llmgateway
else
    echo "Chưa có binary release, tiến hành cargo run --release..."
    exec cargo run --release
fi
