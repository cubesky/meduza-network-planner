# Clash 代理缓慢问题诊断和优化方案

## 问题分析

### 🔴 主要性能瓶颈

#### 1. DNS 查询链路过长

**当前流程**:
```
应用 → dnsmasq:53 → MosDNS:1153 → Clash:1053 → 上游DNS
```

**延迟分析**:
- dnsmasq: ~1-2ms
- MosDNS: ~5-10ms (如果有规则匹配)
- Clash DNS: ~2-5ms
- **总延迟: 8-17ms** (仅 DNS!)

**建议优化**:
```
应用 → MosDNS:1153 → 上游DNS (跳过 dnsmasq 和 Clash DNS)
```

#### 2. MosDNS 配置问题

**检查项**:
```bash
# 查看 MosDNS 配置
docker compose exec meduza cat /etc/mosdns/config.yaml

# 查看 MosDNS 日志
docker compose exec meduza tail -f /var/log/mosdns.out.log

# 测试 DNS 查询延迟
time docker compose exec meduza nslookup google.com 127.0.0.1:1153
```

#### 3. TPROXY 规则过多

**问题**: 所有 TCP/UDP 流量都经过 TPROXY 检查

**检查**:
```bash
# 查看 TPROXY 规则数量
iptables -t mangle -L CLASH_TPROXY -n --line-numbers | wc -l

# 查看流量统计
iptables -t mangle -L CLASH_TPROXY -v -n | head -20
```

#### 4. Clash 配置问题

**可能的性能杀手**:
- `find-process-mode` 不是 `off`
- `sniffer` 开启
- 过多的 `rule-providers`
- 复杂的 `rules`
- `geodata-mode` 不是 `standard`

## 🚀 优化方案

### 方案 1: 简化 DNS 链路 (推荐)

#### 修改 DNS 架构

**步骤 1**: 修改 `[watcher.py:1145-1175](watcher.py#L1145-L1175)` 中的 dnsmasq 配置

```python
# 当使用 MosDNS 时,只转发必要查询
if clash_enabled and mosdns_enabled:
    # 方案 A: dnsmasq 只处理本地查询
    forward_servers = "127.0.0.1#1153"  # 只转发到 MosDNS
else:
    # 方案 B: 标准 fallback
    forward_servers = """
        127.0.0.1#1153
        127.0.0.1#1053
        223.5.5.5
        119.29.29.29
    """
```

**步骤 2**: 修改 Clash 配置禁用内置 DNS

在 `[generators/gen_clash.py](generators/gen_clash.py)` 中:

```python
# 当使用 MosDNS 时,禁用 Clash DNS
if mosdns_enabled:
    clash_config["dns"] = {
        "enable": false,  # 禁用 Clash DNS
        "enhanced-mode": "redir-host"
    }
```

### 方案 2: 优化 Clash 性能配置

#### 必须优化的设置

在 Clash 配置中添加:

```yaml
# 性能优化
find-process-mode: off       # 必须关闭
sniffer: false                # 关闭嗅探
geoip-mode: false             # 禁用 GeoIP
geodata-loader: standard      # 使用标准加载器

# DNS 优化
dns:
  enable: true
  enhanced-mode: redir-host
  prefer-h3: false            # 禁用 h3 以提升性能
  fake-ip-range: 198.18.0.0/16
  fake-ip-filter:
    - '*.lan'
    - 'localhost.ptlogin2.qq.com'
  nameserver:
    - 127.0.0.1:1153          # 使用 MosDNS
  fallback:
    - https://1.1.1.1/dns-query
    - https://8.8.8.8/dns-query
```

### 方案 3: TPROXY 规则优化

#### 添加更多排除规则

**当前排除**:
- 本地地址段 (RFC1918)
- Clash 端口 (7893)

**建议新增**:
```bash
# DNS 查询
iptables -t mangle -A CLASH_TPROXY -p udp --dport 53 -j RETURN
iptables -t mangle -A CLASH_TPROXY -p tcp --dport 53 -j RETURN

# NTP
iptables -t mangle -A CLASH_TPROXY -p udp --dport 123 -j RETURN

# 本地网络广播
iptables -t mangle -A CLASH_TPROXY -d 255.255.255.255 -j RETURN
iptables -t mangle -A CLASH_TPROXY -d 224.0.0.251 -j RETURN  # mDNS
```

### 方案 4: 使用 Clash Meta 的 Fake-IP 优化

```yaml
dns:
  enhanced-mode: fake-ip      # 使用 fake-ip 模式
  fake-ip-range: 198.18.0.0/16
  fake-ip-filter:
    - '*.lan'
    - '*.local'
    - '*.localdomain'
```

**优点**:
- DNS 查询只查一次
- 后续连接直接用 fake-IP,无需再查 DNS

## 🔍 诊断命令

### 1. 检查 DNS 延迟

```bash
# 测试 dnsmasq
time docker compose exec meduza nslookup google.com 127.0.0.1:53

# 测试 MosDNS
time docker compose exec meduza nslookup google.com 127.0.0.1:1153

# 测试 Clash DNS
time docker compose exec meduza nslookup google.com 127.0.0.1:1053

# 测试外部 DNS
time docker compose exec meduza nslookup google.com 223.5.5.5
```

### 2. 检查流量统计

```bash
# TPROXY 流量
iptables -t mangle -L CLASH_TPROXY -v -n

# Clash 连接数
docker compose exec meduza netstat -an | grep :7893 | wc -l

# MosDNS 查询统计
docker compose exec meduza tail -100 /var/log/mosdns.out.log | grep -i "query"
```

### 3. 检查 Clash 性能

```bash
# Clash API
curl http://127.0.0.1:9090/connections

# 查看延迟
curl http://127.0.0.1:9090/proxies
```

## 📊 性能基准

### 预期延迟

- **DNS 查询**: < 10ms
- **HTTP 连接建立**: < 100ms
- **HTTPS 握手**: < 200ms
- **首字节时间 (TTFB)**: < 300ms

### 如果超过预期

1. **DNS > 20ms** → 简化 DNS 链路
2. **连接建立 > 200ms** → 检查代理服务器质量
3. **大量超时** → 检查 TPROXY 规则

## 🎯 立即可做的优化

### 1. 修改 Clash 配置

在订阅配置中添加 (通过 etcd):

```bash
etcdctl put /global/clash/clash_config_mode "performance"
```

或在 Clash 配置 YAML 中强制设置:

```yaml
find-process-mode: off
sniffer: false
```

### 2. 检查 MosDNS 规则

```bash
# 查看规则数量
docker compose exec meduza wc -l /etc/mosdns/config.yaml

# 禁用不必要的插件
etcdctl put /global/mosdns/plugins "[]"
```

### 3. 检查 dnsmasq 转发配置

```bash
docker compose exec meduza cat /etc/dnsmasq.conf | grep -A 10 "server="
```

如果看到多个 server,考虑减少。

## 🔧 实施步骤

### 步骤 1: 诊断 (当前)

运行所有诊断命令,收集数据。

### 步骤 2: 简化 DNS 链

修改 watcher.py,让 dnsmasq 直接转发到 MosDNS。

### 步骤 3: 优化 Clash 配置

强制设置性能优化选项。

### 步骤 4: 测试

```bash
# 重建容器
docker compose build
docker compose up -d

# 测试速度
curl -w "@-" -o /dev/null -s "https://www.google.com" <<EOF
    time_namelookup:  %{time_namelookup}\n
    time_connect:     %{time_connect}\n
    time_appconnect:  %{time_appconnect}\n
    time_pretransfer: %{time_pretransfer}\n
    time_starttransfer: %{time_starttransfer}\n
    time_total:       %{time_total}\n
EOF
```

## 💡 快速修复

如果不想修改代码,可以立即通过 etcd 优化:

```bash
# 1. 禁用 MosDNS (减少 DNS 链路)
etcdctl put /nodes/<NODE_ID>/mosdns/enable "false"
etcdctl put /commit "$(date +%s)"

# 2. 优化 Clash 配置
# 手动编辑 /etc/clash/config.yaml,添加:
# find-process-mode: off
# sniffer: false

# 3. 重启 Clash
docker compose restart meduza
```
