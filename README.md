# Meduza Network Planner 覆盖网络规划网关

Meduza 是一个单容器的边缘网关，使用 etcd 作为唯一配置源，负责协调：

- EasyTier 或 Tinc（覆盖网络，二选一）
- FRR（OSPF + BGP 路由分发）
- OpenVPN（对外互联）
- Clash Meta（透明代理）
- MosDNS（外部调用的 DNS 服务）

## 快速开始

```bash
docker compose build
docker compose up -d
```

必需环境变量：

- NODE_ID
- ETCD_ENDPOINTS
- ETCD_CA / ETCD_CERT / ETCD_KEY
- ETCD_USER / ETCD_PASS

更新 etcd 键后，触发 `/commit` 生效。

## 核心约定

- 只监听 `/commit`，其余键仅作为数据源读取。
- 覆盖网络类型由 `/global/mesh_type` 控制：`easytier` 或 `tinc`（全局二选一）。
- Clash 使用 iptables tproxy 引导流量，不启用 tun。
- MosDNS 仅作为外部调用服务，不修改系统 DNS。

## 运行环境

- 容器使用 `host` 网络模式或 macvlan/ipvlan，作为覆盖网络路由节点。
- 建议为容器提供对外可达的端口与路由策略（FRR 依赖）。

## 透明代理行为

- tproxy 模式只挂接 PREROUTING，不处理 OUTPUT，避免代理本机流量。
- 可通过 `/nodes/<NODE_ID>/clash/exclude_tproxy_port` 跳过端口转发流量。
- 主网关来源可通过 `DEFAULT_GW` 环境变量跳过代理。

## 状态上报

```
/updated/<NODE_ID>/online  = "1"                           # TTL/lease
/updated/<NODE_ID>/last    = "<YYYY-MM-DDTHH:mm:ss+0000>"  # UTC 时间
/updated/<NODE_ID>/<tool>/status = "<state> <YYYY-MM-DDTHH:mm:ss+0000>"
```

OpenVPN 多实例状态：

```
/updated/<NODE_ID>/openvpn/<NAME>/status = "<state> <YYYY-MM-DDTHH:mm:ss+0000>"
```

## 目录结构

- `watcher.py`：调度器，监听 `/commit` 并触发各工具生成与 reload
- `generators/`：独立配置生成脚本
- `scripts/`：运行与系统脚本
- `frr/`、`clash/`、`mosdns/`：各工具模板或默认文件
- `docs/`：详细说明与键值约定

## 文档

- `docs/architecture.md`
- `docs/etcd-schema.md`
- `docs/mosdns.md`
