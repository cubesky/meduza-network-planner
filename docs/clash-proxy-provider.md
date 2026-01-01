# Clash Proxy-Provider 自动处理

## 功能概述

Clash 启动时会自动处理 `proxy-provider`,实现以下功能:

1. **下载远程配置**: 将 proxy-provider 中的 URL 下载到本地 `/etc/clash/providers/`
2. **提取代理服务器 IP**: 解析所有代理服务器的 IP 地址
3. **创建 ipset**: 将所有代理服务器 IP 添加到名为 `proxy-servers` 的 ipset
4. **配置本地路径**: 修改配置使用本地文件而非远程 URL
5. **添加 iptables 规则**: 自动跳过代理服务器 IP 的 TPROXY 规则

## 工作流程

```
Clash 启动
    ↓
1. 下载 GeoX 文件 (geosite/geoip)
    ↓
2. 执行 preprocess-clash.py
    ├─ 读取 /etc/clash/config.yaml
    ├─ 遍历 proxy-providers
    ├─ 下载每个 provider URL 到本地
    ├─ 解析代理配置,提取服务器 IP
    └─ 保存 IP 列表到 /etc/clash/providers/proxy_servers.txt
    ↓
3. 检查 IP 列表文件
    ├─ 如果存在:
    │   ├─ 创建 ipset: proxy-servers
    │   ├─ 添加所有 IP 到 ipset
    │   └─ 添加 iptables 规则跳过这些 IP
    └─ 如果不存在: 跳过
    ↓
4. 启动 mihomo
```

## iptables 规则

添加的规则顺序 (在 CLASH_TPROXY 链开头):

```bash
# 跳过来自代理服务器的流量 (源 IP)
iptables -t mangle -I CLASH_TPROXY -m set --match-set proxy-servers src -j RETURN

# 跳过发往代理服务器的流量 (目标 IP)
iptables -t mangle -I CLASH_TPROXY -m set --match-set proxy-servers dst -j RETURN
```

这些规则确保:
- 从代理服务器发来的流量不会被代理
- 发往代理服务器的流量不会被代理
- 避免代理循环

## 文件结构

```
/etc/clash/
├── config.yaml              # Clash 配置 (会被修改)
├── providers/               # Provider 本地文件目录
│   ├── provider1.yml        # 下载的 provider 配置
│   ├── provider2.yml
│   ├── proxy_servers.txt    # 提取的 IP 列表
│   └── proxy_servers.json   # IP 列表 (JSON 格式)
└── ui/                      # MetaCubeXD 界面
```

## 配置示例

### 原始配置

```yaml
proxy-providers:
  provider1:
    type: http
    url: "https://example.com/provider1.yml"
    interval: 3600
    path: ./provider1.yml
    health-check:
      enable: true
      interval: 600
      url: http://www.gstatic.com/generate_204
```

### 启动后自动转换为

```yaml
proxy-providers:
  provider1:
    type: file
    url: "file:///etc/clash/providers/provider1.yml"
    path: /etc/clash/providers/provider1.yml
    interval: 3600
    health-check:
      enable: true
      interval: 600
      url: http://www.gstatic.com/generate_204
```

## IP 提取逻辑

脚本会从以下位置提取 IP:

1. **proxy-provider 配置中的 proxies**: 下载远程配置后提取
2. **本地配置中的 proxies**: 直接从 `config.yaml` 的 `proxies` 字段提取

### 支持的协议

- ss (Shadowsocks)
- ssr (ShadowsocksR)
- vmess
- vless
- trojan
- snell
- socks5

### IP 解析优先级

1. 如果 `server` 字段是 IP 地址 → 直接使用
2. 如果 `server` 字段是域名 → 使用 `getent hosts` 解析
3. 支持解析多个 IP (域名对应多个 A 记录)

## 调试

### 查看下载的 provider 文件

```bash
ls -la /etc/clash/providers/
cat /etc/clash/providers/provider1.yml
```

### 查看 IP 列表

```bash
cat /etc/clash/providers/proxy_servers.txt
```

### 查看 ipset

```bash
ipset list proxy-servers
```

### 查看 iptables 规则

```bash
iptables -t mangle -L CLASH_TPROXY -n --line-numbers
```

### 测试特定域名解析

```bash
getent hosts example.com
```

## 故障排查

### provider 下载失败

检查日志:
```bash
docker compose exec meduza tail -100 /var/log/mihomo.out.log | grep -i "download\|provider"
```

手动测试下载:
```bash
curl -fL "https://example.com/provider.yml"
```

### IP 列表为空

1. 检查配置是否有 proxy-provider:
```bash
docker compose exec meduza cat /etc/clash/config.yaml | grep -A 5 "proxy-providers"
```

2. 检查预处理脚本输出:
```bash
docker compose exec meduza python3 /usr/local/bin/preprocess-clash.py /etc/clash/config.yaml /tmp/providers/
```

3. 查看提取的 IP:
```bash
cat /etc/clash/providers/proxy_servers.txt
```

### iptables 规则未生效

检查规则是否存在:
```bash
iptables -t mangle -C CLASH_TPROXY -m set --match-set proxy-servers src -j RETURN; echo $?
```

如果返回非 0,规则不存在。检查日志:
```bash
docker compose logs mihomo 2>&1 | grep -i "iptables\|ipset"
```

## 限制和注意事项

1. **域名解析**: 使用容器中的 DNS,依赖 `/etc/resolv.conf`
2. **IPv6**: 当前只处理 IPv4 地址
3. **动态更新**: provider 更新后需要重启 Clash 才能重新提取 IP
4. **网络依赖**: 首次启动需要网络连接下载 provider

## 性能影响

- **启动时间**: 增加 5-10 秒 (取决于 provider 数量和网络速度)
- **内存占用**: ipset 每个占用约 100 字节,1000 个 IP 约 100KB
- **iptables 规则**: 每条规则约 50 字节,2 条规则约 100 字节

## 与 TPROXY 的集成

这些规则在 TPROXY 规则**之前**处理,确保:

```
流量 → iptables mangle → CLASH_TPROXY 链
                     ↓
             1. 检查 src IP 在 proxy-servers? → RETURN (跳过)
             2. 检查 dst IP 在 proxy-servers? → RETURN (跳过)
             3. 其他 TPROXY 排除规则...
             4. 最后才是 TPROXY 代理规则
```

这样确保代理服务器的连接永远不会被代理,避免死循环。
