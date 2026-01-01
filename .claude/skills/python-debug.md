# Python 调试技能指南

## 概述

本项目需要在容器环境中运行 Python 脚本。由于使用 uv 作为 Python 包管理器，**所有 Python 调试操作必须通过 uv 调用**。

## 正确的 Python 调试方法

### 语法检查

```bash
# 检查 Python 语法
uv run python -m py_compile watcher.py

# 检查特定语法错误
uv run python -c "import ast; ast.parse(open('watcher.py').read())"
```

### 代码调试

```bash
# 运行 Python 脚本
uv run python watcher.py --help

# 带参数运行
uv run python generators/gen_clash.py <<< '{"node_id":"test","node":{},"global":{},"all_nodes":{}}'

# 检查导入
uv run python -c "import watcher; print('Import successful')"
```

### 单元测试

```bash
# 运行测试（如果存在）
uv run pytest tests/

# 运行特定测试
uv run pytest tests/test_watcher.py -v
```

### 交互式调试

```bash
# 启动交互式 Python shell
uv run python

# 带断点调试
uv run python -m pdb watcher.py

# IPython 调试
uv run ipython
```

## 常见错误

### ❌ 错误方式

```bash
# 这些命令在容器中可能失败或使用错误的 Python 环境
python watcher.py
python3 -m py_compile watcher.py
pytest tests/
```

### ✅ 正确方式

```bash
# 始终使用 uv run
uv run python watcher.py
uv run python -m py_compile watcher.py
uv run pytest tests/
```

## 原因

1. **环境隔离**: uv 管理独立的 Python 虚拟环境
2. **依赖管理**: 确保使用正确的依赖版本
3. **一致性**: 开发和生产环境使用相同的 Python 环境
4. **避免冲突**: 防止系统 Python 或其他 Python 环境干扰

## 检查 Python 环境

```bash
# 查看当前 Python 路径
uv run python -c "import sys; print(sys.executable)"

# 查看已安装的包
uv run pip list

# 查看 Python 版本
uv run python --version
```

## 调试技巧

### 1. 快速语法检查

```bash
uv run python -m py_compile watcher.py && echo "✓ Syntax OK" || echo "✗ Syntax Error"
```

### 2. 检查导入

```bash
uv run python -c "
import sys
try:
    import watcher
    print('✓ All imports successful')
except ImportError as e:
    print(f'✗ Import error: {e}')
    sys.exit(1)
"
```

### 3. 静态分析（如果安装了相关工具）

```bash
# 类型检查
uv run mypy watcher.py

# 代码质量检查
uv run pylint watcher.py

# 格式检查
uv run black --check watcher.py
```

## 容器内调试

如果需要在运行的容器中调试：

```bash
# 进入容器
docker compose exec meduza bash

# 在容器内运行（如果容器安装了 uv）
uv run python -c "print('Debugging in container')"

# 或者直接使用容器中的 Python
python3 -c "print('Direct container Python')"
```

## 相关文档

- [uv 官方文档](https://github.com/astral-sh/uv)
- [Python 调试指南](https://docs.python.org/3/library/pdb.html)
- 项目 README
- CLAUDE.md
