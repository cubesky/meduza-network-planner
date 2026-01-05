# s6-overlay Migration - Final Checklist

## Pre-Build Verification

- [x] Removed `supervisor` package from Dockerfile
- [x] Added s6-overlay installation to Dockerfile
- [x] Removed supervisord.conf from COPY command
- [x] Added s6-services directory to Dockerfile
- [x] Updated run script permissions in Dockerfile
- [x] All s6 service run scripts have correct shebang: `#!/command/execlineb -P`
- [x] Service dependencies configured correctly
- [x] Default bundle includes all required services

## Code Changes

- [x] Replaced all `_supervisor_*` functions with `_s6_*` equivalents
- [x] Updated service status checks ("RUNNING" → "up")
- [x] Added dynamic service management functions
- [x] Updated OpenVPN dynamic service creation
- [x] Updated WireGuard dynamic service creation
- [x] Fixed s6_retry_loop for s6-overlay behavior
- [x] Updated environment variable names (SUPERVISOR_RETRY_INTERVAL → S6_RETRY_INTERVAL)
- [x] Updated all comments and documentation

## Documentation Updates

- [x] Updated CLAUDE.md with s6-overlay references
- [x] Created S6-MIGRATION.md technical documentation
- [x] Created MIGRATION-GUIDE.md for users
- [x] Removed old supervisord.conf file

## Build and Test Steps

### 1. Build the image
```bash
docker compose build
```

Expected: No errors during build

### 2. Start the container
```bash
docker compose up -d
docker compose logs -f
```

Expected: Container starts successfully, no crash loops

### 3. Check s6 services
```bash
docker compose exec meduza s6-rc -a list
```

Expected output should include:
- dbus
- avahi
- watchfrr
- watcher
- mihomo
- dns-monitor

### 4. Verify watcher is running
```bash
docker compose exec meduza ps aux | grep watcher
docker compose exec meduza tail -f /var/log/watcher.out.log
```

Expected: Watcher process running, no errors in logs

### 5. Test etcd triggers
```bash
etcdctl put /commit "$(date +%s)"
```

Expected: Configuration change triggers reconciliation

### 6. Test dynamic service (EasyTier)
```bash
# Enable EasyTier
etcdctl put /nodes/<NODE_ID>/easytier/enable "true"
etcdctl put /global/mesh_type "easytier"
etcdctl put /commit "$(date +%s)"

# Check service started
docker compose exec meduza s6-rc -a list | grep easytier
```

Expected: EasyTier service appears in active services

### 7. Test dynamic service (Tinc)
```bash
# Switch to Tinc
etcdctl put /global/mesh_type "tinc"
etcdctl put /nodes/<NODE_ID>/tinc/enable "true"
etcdctl put /commit "$(date +%s)"

# Verify EasyTier stopped, Tinc started
docker compose exec meduza s6-rc -a list | grep -E "(easytier|tinc)"
```

Expected: Only Tinc is running, EasyTier stopped

### 8. Test OpenVPN/WireGuard (if configured)
```bash
# Add OpenVPN config via etcd
etcdctl put /nodes/<NODE_ID>/openvpn/test/enable "true"
etcdctl put /commit "$(date +%s)"

# Check dynamic service created
docker compose exec meduza ls -la /etc/s6-overlay/sv/ | grep openvpn
docker compose exec meduza s6-rc -a list | grep openvpn
```

Expected: Dynamic service created and started

### 9. Test service restart
```bash
docker compose exec meduza s6-rc restart mihomo
```

Expected: Service restarts successfully

### 10. Check logs for all services
```bash
docker compose exec meduza tail -100 /var/log/*.log
```

Expected: No critical errors in any service logs

### 11. Test MosDNS/Dnsmasq
```bash
etcdctl put /nodes/<NODE_ID>/mosdns/enable "true"
etcdctl put /commit "$(date +%s)"

# Check services
docker compose exec meduza s6-rc -a list | grep -E "(mosdns|dnsmasq)"
```

Expected: Both MosDNS and dnsmasq running

### 12. Test DNS resolution
```bash
docker compose exec meduza nslookup google.com
```

Expected: DNS resolution works

## Rollback Plan

If any critical issues are found:

1. Stop and remove container: `docker compose down`
2. Checkout previous commit: `git checkout <pre-migration-commit>`
3. Rebuild: `docker compose build`
4. Start: `docker compose up -d`

## Known Limitations

1. s6-overlay doesn't have "FATAL" state like supervisord
   - Retry logic simplified in s6_retry_loop()
   - s6 handles most restarts automatically

2. Dynamic services require recompilation
   - `_s6_reload_services()` must be called after adding/removing services
   - This is handled automatically in reload_openvpn() and reload_wireguard()

3. Service status is simpler
   - Only "up" or "down" (vs "RUNNING", "STOPPED", "FATAL", etc.)
   - Status checking logic updated accordingly

## Performance Monitoring

After migration, monitor for:
- Container startup time (should be faster)
- Memory usage (should be slightly lower)
- Service restart frequency (should be more reliable)
- CPU usage during restarts (should be lower)

## Success Criteria

Migration is successful if:
- ✅ All services start correctly
- ✅ Etcd triggers work as expected
- ✅ Dynamic services (EasyTier/Tinc) start/stop properly
- ✅ OpenVPN/WireGuard instances are created dynamically
- ✅ Service restarts work correctly
- ✅ No increase in error logs
- ✅ System is stable for 24+ hours
