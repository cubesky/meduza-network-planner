#!/bin/bash
# 快速修复调试工具 - 重新构建并验证
# 支持 Docker 和 Podman

set -euo pipefail

# 检测使用的平台
if command -v podman >/dev/null 2>&1 && podman compose version >/dev/null 2>&1; then
    COMPOSE="podman compose"
    PLATFORM="Podman"
elif command -v docker >/dev/null 2>&1; then
    COMPOSE="docker compose"
    PLATFORM="Docker"
else
    echo "Error: Neither docker nor podman found" >&2
    exit 1
fi

echo "=== 修复调试工具 (${PLATFORM}) ===" >&2
echo "" >&2

echo "1. 停止容器..." >&2
$COMPOSE down

echo "" >&2
echo "2. 重新构建镜像 (不使用缓存)..." >&2
$COMPOSE build --no-cache

echo "" >&2
echo "3. 启动容器..." >&2
$COMPOSE up -d

echo "" >&2
echo "4. 等待服务启动 (10秒)..." >&2
sleep 10

echo "" >&2
echo "5. 验证工具..." >&2
if $COMPOSE exec meduza which get-logs >/dev/null 2>&1; then
    echo "✅ get-logs 找到: $($COMPOSE exec meduza which get-logs)" >&2
else
    echo "❌ get-logs 未找到" >&2
    exit 1
fi

if $COMPOSE exec meduza which get-services >/dev/null 2>&1; then
    echo "✅ get-services 找到: $($COMPOSE exec meduza which get-services)" >&2
else
    echo "❌ get-services 未找到" >&2
    exit 1
fi

echo "" >&2
echo "6. 测试工具..." >&2
echo "" >&2
echo "运行 get-services:" >&2
$COMPOSE exec meduza get-services | head -20

echo "" >&2
echo "=== 修复完成! ===" >&2
echo "" >&2
echo "现在可以使用:" >&2
echo "  $COMPOSE exec meduza get-services" >&2
echo "  $COMPOSE exec meduza get-logs watcher" >&2
echo "  $COMPOSE exec meduza get-logs -f mihomo" >&2
