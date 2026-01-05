#!/bin/bash
# s6-overlay 服务修复和重新构建脚本
# 自动修复并重新构建容器以应用 s6-overlay v3 配置

set -e

echo "=== s6-overlay 服务修复和部署 ===" >&2
echo "" >&2

# 检测容器运行时
if command -v docker &> /dev/null; then
    RUNTIME="docker"
    COMPOSE="docker compose"
elif command -v podman &> /dev/null; then
    RUNTIME="podman"
    COMPOSE="podman-compose"
else
    echo "❌ 错误: 未找到 docker 或 podman" >&2
    exit 1
fi

echo "📦 检测到容器运行时: $RUNTIME" >&2
echo "" >&2

# 1. 停止容器
echo "1. 停止容器..." >&2
$COMPOSE down 2>/dev/null || true
echo "   ✅ 容器已停止" >&2
echo "" >&2

# 2. 重新构建镜像 (不使用缓存)
echo "2. 重新构建镜像 (不使用缓存)..." >&2
$COMPOSE build --no-cache
echo "   ✅ 镜像构建完成" >&2
echo "" >&2

# 3. 启动容器
echo "3. 启动容器..." >&2
$COMPOSE up -d
echo "   ✅ 容器已启动" >&2
echo "" >&2

# 4. 等待服务启动
echo "4. 等待服务启动 (15秒)..." >&2
sleep 15
echo "   ✅ 等待完成" >&2
echo "" >&2

# 5. 验证工具
echo "5. 验证调试工具..." >&2
if $COMPOSE exec -T meduza which get-logs >/dev/null 2>&1; then
    echo "   ✅ get-logs 找到: $($COMPOSE exec -T meduza which get-logs)" >&2
else
    echo "   ❌ get-logs 未找到" >&2
fi

if $COMPOSE exec -T meduza which get-services >/dev/null 2>&1; then
    echo "   ✅ get-services 找到: $($COMPOSE exec -T meduza which get-services)" >&2
else
    echo "   ❌ get-services 未找到" >&2
fi
echo "" >&2

# 6. 检查 s6 服务
echo "6. 检查 s6 服务状态..." >&2
echo "   已编译的服务:" >&2
$COMPOSE exec -T meduza s6-rc -a 2>/dev/null || echo "   ⚠️  无法列出服务" >&2
echo "" >&2

# 7. 测试工具
echo "7. 测试 get-services 工具..." >&2
$COMPOSE exec meduza get-services 2>&1 || echo "   ⚠️  get-services 执行失败" >&2
echo "" >&2

echo "=== 部署完成 ===" >&2
echo "" >&2
echo "📝 常用命令:" >&2
echo "  查看服务状态: $COMPOSE exec meduza get-services" >&2
echo "  查看服务日志: $COMPOSE exec meduza get-logs <service>" >&2
echo "  跟踪日志:     $COMPOSE exec meduza get-logs -f <service>" >&2
echo "  进入容器:     $COMPOSE exec meduza bash" >&2
echo "" >&2
