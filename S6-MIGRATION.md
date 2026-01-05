# s6-overlay Migration

This document describes the migration from supervisord to s6-overlay for process supervision.

## Overview

s6-overlay is a process supervision and service management suite that provides better reliability and control over containerized services. It replaces the previous supervisord-based setup.

## Architecture

### Service Location

All s6 service definitions are located in `/etc/s6-overlay/sv/<service-name>/`:

```
/etc/s6-overlay/sv/
├── dbus/              # D-Bus system service (highest priority)
├── avahi/             # mDNS/DNS-SD (depends on dbus)
├── watchfrr/          # FRR routing daemon supervisor
├── watcher/           # Main orchestration service (depends on dbus, avahi, watchfrr)
├── mihomo/            # Clash Meta proxy
├── dns-monitor/       # DNS configuration monitor
├── easytier/          # EasyTier mesh network (on-demand)
├── tinc/              # Tinc VPN (on-demand)
├── mosdns/            # DNS forwarder (on-demand)
└── dnsmasq/           # DNS caching server (on-demand)
```

### Dynamic Services

OpenVPN and WireGuard instances are created dynamically by the watcher:

```
/etc/s6-overlay/sv/
├── openvpn-<name>/    # Dynamic OpenVPN instances
└── wireguard-<name>/  # Dynamic WireGuard instances
```

### Service Structure

Each service directory contains:
- `run` - Service startup script (required, executable)
- `finish` - Service cleanup script (optional)
- `dependencies.d/` - Dependency files (optional)

## Startup Sequence

1. **dbus** - System message bus (no dependencies)
2. **avahi** - mDNS responder (depends on dbus)
3. **watchfrr** - FRR supervisor (no dependencies)
4. **watcher** - Main orchestrator (depends on dbus, avahi, watchfrr)
5. **mihomo** - Clash proxy (started by default bundle)
6. **dns-monitor** - DNS monitor (started by default bundle)

### On-Demand Services

These services are started/stopped by the watcher based on etcd configuration:
- `easytier` - When `/nodes/<NODE_ID>/easytier/enable = "true"`
- `tinc` - When `/nodes/<NODE_ID>/tinc/enable = "true"`
- `mosdns` - When `/nodes/<NODE_ID>/mosdns/enable = "true"`
- `dnsmasq` - Started with mosdns

## Default Bundle

The `default` bundle specifies which services start automatically:
- Contents: `dbus`, `avahi`, `watchfrr`, `watcher`, `mihomo`, `dns-monitor`

## API Changes

### Python (watcher.py)

Old (supervisord):
```python
_supervisor_status("service")
_supervisor_start("service")
_supervisor_stop("service")
_supervisor_restart("service")
_supervisor_is_running("service")
_supervisor_status_all()
```

New (s6-overlay):
```python
_s6_status("service")      # Returns "up" or "down"
_s6_start("service")
_s6_stop("service")
_s6_restart("service")
_s6_is_running("service")  # Returns True/False
_s6_status_all()          # Returns Dict[str, str]
```

### Dynamic Service Management

New functions added:
```python
_s6_create_dynamic_service(name, command)  # Create service directory
_s6_remove_dynamic_service(name)           # Remove service directory
_s6_reload_services()                      # Recompile services database
```

## Service Control

### Check service status:
```bash
s6-rc -a list                    # List all active services
s6-rc list all                   # List all known services
```

### Manual control:
```bash
s6-rc -u start <service>         # Start a service
s6-rc -d stop <service>          # Stop a service
s6-rc restart <service>          # Restart a service
```

## Initialization

The entrypoint script now:
1. Sets up required directories
2. Runs DNS monitor once
3. Configures FRR
4. Compiles s6 services database: `s6-rc-compile /etc/s6-overlay/compiled /etc/s6-overlay/sv`
5. Starts s6-overlay: `exec /init`

## Benefits of s6-overlay

1. **Reliability**: Better signal handling and process supervision
2. **Performance**: Lower overhead than supervisord
3. **Simplicity**: Clean service definition format
4. **Portability**: Container-native init system
5. **Dependency Management**: Native service dependency support

## Troubleshooting

### View service logs:
```bash
# Logs are still in their original locations
tail -f /var/log/<service>.*.log
```

### Check if service is running:
```bash
s6-rc -a list | grep <service>
```

### Restart a service:
```bash
s6-rc restart <service>
```

### Reload configuration after adding/removing services:
```bash
# This is done automatically by watcher.py
s6-rc-compile /etc/s6-overlay/compiled /etc/s6-overlay/sv
```

## Migration Notes

- All service behavior remains the same from an operational perspective
- etcd triggers work identically
- Service startup order is preserved via dependencies
- Logging locations unchanged
- No changes to generator scripts or configuration files
