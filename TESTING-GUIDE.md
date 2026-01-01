# Testing Guide - Clash LAN Mode

## Quick Test Procedure

### 1. Build and Deploy

```bash
# Build container with LAN mode support
docker compose build

# Start services
docker compose up -d

# Check service status
docker compose exec meduza s6-rc -a
```

### 2. Configure LAN Mode

```bash
# Example: Configure single LAN segment
export NODE_ID="gateway1"
etcdctl put /nodes/${NODE_ID}/lan "10.42.0.0/24"
etcdctl put /nodes/${NODE_ID}/clash/mode "tproxy"
etcdctl put /commit "$(date +%s)"
```

### 3. Verify LAN Mode is Active

```bash
# Check watcher logs for LAN mode activation
docker compose exec meduza tail -100 /var/log/watcher.out.log | grep -i "lan mode"

# Should see: "[TPROXY] LAN MODE enabled - only proxying traffic from: 10.42.0.0/24"
```

### 4. Inspect iptables Rules

```bash
# View CLASH_TPROXY chain rules
docker compose exec meduza iptables -t mangle -L CLASH_TPROXY -n --line-numbers

# Look for:
# - ACCEPT rules for LAN sources
# - RETURN rules to reject non-LAN traffic
# - TPROXY rules only for LAN sources

# Get rule statistics
docker compose exec meduza iptables -t mangle -L CLASH_TPROXY -v -n | head -20
```

### 5. Test Proxy from LAN Client

```bash
# From a LAN client (10.42.0.x), test internet access
curl -w "@-" -o /dev/null -s "https://www.google.com" <<'EOF'
    time_namelookup:  %{time_namelookup}\n
    time_connect:     %{time_connect}\n
    time_appconnect:  %{time_appconnect}\n
    time_starttransfer: %{time_starttransfer}\n
    time_total:       %{time_total}\n
EOF

# Expected: All times should be reasonable (< 1s for time_total)
```

### 6. Verify Server Traffic is NOT Proxied

```bash
# From the gateway server itself, test external access
docker compose exec meduza curl -w "@-" -o /dev/null -s "https://www.google.com" <<'EOF'
    time_total:  %{time_total}\n
EOF

# This should NOT go through the proxy (direct connection)
```

### 7. Run Performance Diagnostics

```bash
# Run diagnostic script
docker compose exec meduza bash /scripts/diagnose-clash-perf.sh

# Check key metrics:
# - DNS latency (should be < 10ms)
# - TPROXY rule count
# - Active connections
# - Proxy speed test
```

### 8. Monitor Traffic Distribution

```bash
# View TPROXY traffic statistics
docker compose exec meduza iptables -t mangle -L CLASH_TPROXY -v -n --line-numbers

# Compare packet counts:
# - Early RETURN rules (non-LAN traffic rejected)
# - ACCEPT rules (LAN traffic accepted)
# - TPROXY rules (actual proxied traffic)

# Expected: Most traffic should hit early RETURN rules
```

## Validation Checklist

- [ ] Container builds successfully
- [ ] All s6 services start correctly (`s6-rc -a` shows expected services)
- [ ] LAN configuration is read from etcd
- [ ] Watcher logs show "LAN MODE enabled"
- [ ] iptables rules include ACCEPT for LAN sources
- [ ] iptables rules include final RETURN to reject non-LAN
- [ ] LAN clients can access internet through proxy
- [ ] Server-side traffic is NOT proxied
- [ ] Performance diagnostics show improvement
- [ ] TPROXY statistics show reduced traffic processing

## Expected iptables Rules (LAN Mode)

```
Chain CLASH_TPROXY (1 references)
num pkts bytes target     prot opt in     out     source               destination
1    0   0 RETURN     all  --  eth0    *       0.0.0.0/0            0.0.0.0/0
2    0   0 RETURN     all  --  *      *       192.168.1.1          0.0.0.0/0
3    0   0 RETURN     all  --  *      *       0.0.0.0/0            127.0.0.0/8
4    0   0 RETURN     tcp  --  *      *       0.0.0.0/0            0.0.0.0/0            tcp dpt:53
5    0   0 TPROXY     tcp  --  *      *       10.42.0.0/24         0.0.0.0/0           TPROXY redirect ... <- ONLY LAN
6    0   0 TPROXY     udp  --  *      *       10.42.0.0/24         0.0.0.0/0           TPROXY redirect ...
```

**关键点**:
- 没有 ACCEPT 或最终 RETURN 规则
- 非 LAN 流量不匹配任何规则,自然通过链
- 只有来自 LAN 的流量被 TPROXY 代理

## Performance Comparison Test

### Before (Standard Mode)

```bash
# Configure without LAN sources
etcdctl put /nodes/${NODE_ID}/lan ""
etcdctl put /commit "$(date +%s)"

# Wait for reconfiguration
sleep 5

# Check TPROXY statistics (note high packet counts)
docker compose exec meduza iptables -t mangle -L CLASH_TPROXY -v -n | head -5
```

### After (LAN Mode)

```bash
# Configure with LAN sources
etcdctl put /nodes/${NODE_ID}/lan "10.42.0.0/24"
etcdctl put /commit "$(date +%s)"

# Wait for reconfiguration
sleep 5

# Check TPROXY statistics (should see lower packet counts on TPROXY rules)
docker compose exec meduza iptables -t mangle -L CLASH_TPROXY -v -n | head -5
```

### Compare Results

| Metric | Standard Mode | LAN Mode |
|--------|---------------|----------|
| Total packets entering chain | High | Low |
| Packets hitting RETURN (rule 6) | N/A | High (most traffic rejected) |
| Packets hitting TPROXY | High | Low (only LAN) |
| CPU usage | Higher | Lower |

## Troubleshooting Commands

```bash
# Check if LAN mode is enabled
docker compose exec meduza grep -i "lan mode" /var/log/watcher.out.log

# View all TPROXY rules
docker compose exec meduza iptables -t mangle -L CLASH_TPROXY -n -v

# Check LAN configuration in etcd
etcdctl get /nodes/${NODE_ID}/lan

# Verify Clash is running in TPROXY mode
docker compose exec meduza s6-rc status mihomo

# Test DNS resolution (through MosDNS)
docker compose exec meduza nslookup google.com 127.0.0.1:1153

# Check Clash connections
docker compose exec meduza netstat -an | grep :7893 | wc -l
```

## Success Criteria

✅ **LAN mode is enabled**: Logs show "LAN MODE enabled"
✅ **iptables rules correct**: ACCEPT for LAN, RETURN to reject others
✅ **LAN clients work**: Can access internet through proxy
✅ **Server traffic bypassed**: Server-side traffic not proxied
✅ **Performance improved**: Lower packet counts on TPROXY rules
✅ **No errors**: Clean logs, no iptables errors

## Next Steps After Testing

1. **Monitor performance**: Run diagnostics periodically
2. **Adjust LAN segments**: Add/remove LAN CIDRs as needed
3. **Optimize further**: Review [docs/performance-tuning.md](docs/performance-tuning.md)
4. **Report issues**: Document any unexpected behavior

## Additional Resources

- [CLASH-LAN-MODE-SUMMARY.md](CLASH-LAN-MODE-SUMMARY.md) - Implementation summary
- [docs/clash-lan-mode.md](docs/clash-lan-mode.md) - Complete documentation
- [docs/performance-tuning.md](docs/performance-tuning.md) - Performance guide
- [scripts/diagnose-clash-perf.sh](scripts/diagnose-clash-perf.sh) - Diagnostic tool
