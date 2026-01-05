# Supervisord → s6-overlay Migration Guide

## Changes Summary

### Removed Files
- `supervisord.conf` - No longer needed
- `/etc/supervisor/conf.d/*.conf` - Dynamic services now managed by s6

### New Files
- `s6-services/` - Directory containing all s6 service definitions
  - `s6-services/dbus/run` - D-Bus service
  - `s6-services/avahi/run` - Avahi service (with dependency on dbus)
  - `s6-services/watchfrr/run` - FRR supervisor
  - `s6-services/watcher/run` - Main watcher service (with dependencies)
  - `s6-services/mihomo/run` - Clash Meta proxy
  - `s6-services/dns-monitor/run` - DNS monitor
  - `s6-services/easytier/run` - EasyTier mesh (on-demand)
  - `s6-services/tinc/run` - Tinc VPN (on-demand)
  - `s6-services/mosdns/run` - MosDNS forwarder (on-demand)
  - `s6-services/dnsmasq/run` - DNS cache (on-demand)
  - `s6-services/default/` - Bundle of auto-start services

### Modified Files
- `Dockerfile` - Added s6-overlay installation, removed supervisord
- `entrypoint.sh` - Changed to initialize s6-overlay instead of supervisord
- `watcher.py` - Replaced all supervisor functions with s6 equivalents

## Building the New Image

```bash
docker compose build
```

## Testing Checklist

1. **Container starts successfully**
   ```bash
   docker compose up -d
   docker compose logs -f
   ```

2. **Check s6 services**
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

3. **Verify watcher is running**
   ```bash
   docker compose exec meduza ps aux | grep watcher
   ```

4. **Test etcd triggers**
   ```bash
   etcdctl put /commit "$(date +%s)"
   ```

5. **Check service logs**
   ```bash
   docker compose exec meduza tail -f /var/log/watcher.out.log
   ```

6. **Test dynamic services**
   - Enable EasyTier: `etcdctl put /nodes/<NODE_ID>/easytier/enable "true"`
   - Trigger: `etcdctl put /commit "$(date +%s)"`
   - Check: `docker compose exec meduza s6-rc -a list | grep easytier`

## Rollback Procedure

If you encounter issues and need to revert to supervisord:

1. Checkout previous commit:
   ```bash
   git checkout <previous-commit>
   ```

2. Rebuild:
   ```bash
   docker compose build
   docker compose up -d
   ```

## Troubleshooting

### Service fails to start

Check the service logs:
```bash
docker compose exec meduza cat /var/log/<service>.err.log
```

### s6-rc-compile fails

The entrypoint will exit with error. Check:
```bash
docker compose exec meduza ls -la /etc/s6-overlay/sv/
docker compose exec meduza cat /etc/s6-overlay/sv/*/run
```

All `run` scripts must be executable and have proper shebang: `#!/command/execlineb -P`

### Dynamic service not created

Check watcher logs:
```bash
docker compose exec meduza tail -f /var/log/watcher.out.log
```

Look for errors from `_s6_create_dynamic_service`.

### Service won't stop

Force stop:
```bash
docker compose exec meduza s6-rc -d stop <service>
```

## Performance Improvements

Expected improvements with s6-overlay:
- **Faster startup** - s6 has lower initialization overhead
- **Cleaner shutdown** - Better signal handling and process tree management
- **Resource usage** - Slightly lower memory footprint
- **Reliability** - More robust process supervision

## Compatibility

- All existing etcd configuration works unchanged
- Generator scripts require no modifications
- Service behavior is identical from user perspective
- Log file locations remain the same

## Next Steps

After successful migration:
1. Remove old supervisord.conf from repository
2. Update documentation to reference s6-overlay
3. Update CLAUDE.md with s6 service management commands
4. Monitor system stability for 24-48 hours
