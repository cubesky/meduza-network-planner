# Clash Proxy-Provider 自动处理功能

## ✅ 已完成

### 新增文件

1. **[scripts/preprocess-clash.py](scripts/preprocess-clash.py)** - Clash 配置预处理脚本
   - 下载远程 proxy-provider 到本地
   - 提取所有代理服务器 IP 地址
   - 解析域名到 IP
   - 保存 IP 列表

2. **[docs/clash-proxy-provider.md](docs/clash-proxy-provider.md)** - 功能文档

### 修改文件

1. **[scripts/run-clash.sh](scripts/run-clash.sh)** - 启动脚本
   - 添加 proxy-provider 预处理步骤
   - 创建 ipset 包含代理服务器 IP
   - 添加 iptables 规则跳过代理服务器

2. **[Dockerfile](Dockerfile)** - 添加预处理脚本复制

## 🎯 功能特性

### 1. 自动下载 proxy-provider

```yaml
# 配置
proxy-providers:
  myprovider:
    url: "https://example.com/providers.yml"
```

→ 自动下载到 `/etc/clash/providers/providers.yml`

### 2. IP 地址提取

- 从下载的 provider 配置中提取
- 从本地 `proxies` 中提取
- 支持域名解析 (使用 `getent hosts`)
- 支持所有常见协议 (ss, vmess, trojan, etc.)

### 3. ipset 创建

```bash
ipset create proxy-servers hash:ip
# 添加所有代理服务器 IP
```

### 4. iptables 规则

```bash
# 跳过来自代理服务器的流量
iptables -t mangle -I CLASH_TPROXY -m set --match-set proxy-servers src -j RETURN

# 跳过发往代理服务器的流量
iptables -t mangle -I CLASH_TPROXY -m set --match-set proxy-servers dst -j RETURN
```

## 🔄 工作流程

```
Clash 启动
  ↓
下载 GeoX 文件
  ↓
preprocess-clash.py
  ├─ 读取 config.yaml
  ├─ 遍历 proxy-providers
  ├─ 下载到 /etc/clash/providers/
  ├─ 提取 IP 地址
  └─ 保存到 proxy_servers.txt
  ↓
检查 IP 列表
  ↓
创建 ipset
  ↓
添加 iptables 规则
  ↓
启动 mihomo
```

## 📁 生成的文件

```
/etc/clash/providers/
├── provider1.yml           # 下载的配置
├── provider2.yml
├── proxy_servers.txt       # IP 列表 (每行一个)
└── proxy_servers.json      # IP 列表 (JSON 格式)
```

## 🔧 调试命令

```bash
# 查看 IP 列表
cat /etc/clash/providers/proxy_servers.txt

# 查看 ipset
ipset list proxy-servers

# 查看 iptables 规则
iptables -t mangle -L CLASH_TPROXY -n --line-numbers

# 查看下载的 provider
ls -la /etc/clash/providers/
```

## ⚠️ 注意事项

1. **启动时间**: 增加 5-10 秒 (取决于 provider 数量)
2. **网络依赖**: 首次启动需要网络连接
3. **IPv6**: 当前只处理 IPv4
4. **动态更新**: provider 更新需重启 Clash

## ✅ 语法检查

- ✅ `preprocess-clash.py` - 语法正确
- ✅ `run-clash.sh` - 脚本正确

## 🚀 使用方法

无需额外配置,Clash 启动时自动处理。只需在配置中定义 proxy-provider:

```yaml
proxy-providers:
  myprovider:
    type: http
    url: "https://example.com/provider.yml"
    interval: 3600
```

系统会自动:
1. 下载配置
2. 提取 IP
3. 创建防火墙规则
4. 避免代理循环
