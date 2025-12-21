# MosDNS

MosDNS is optional and controlled per node.

## Enable

Set `/nodes/<NODE_ID>/mosdns/enable` to `true` to start MosDNS on that node.
If the key is missing or not `true`, MosDNS is disabled.

When enabled, the watcher writes `/etc/mosdns/config.yaml`, downloads rule files,
starts MosDNS, and sets `/etc/resolv.conf` to `127.0.0.1`.

## Rule files

Rules are stored in etcd under:

- `/global/mosdns/rule_files`

Value must be a JSON object that maps relative file paths to URLs:

```json
{
  "ddns.txt": "https://profile.kookxiang.com/rules/mosdns/ddns.txt",
  "block.txt": "https://profile.kookxiang.com/rules/mosdns/block.txt",
  "geosite/private.txt": "https://profile.kookxiang.com/geosite/domains/private"
}
```

Downloaded files are written to `/etc/mosdns/<path>`.

## Plugins

Plugins are stored in etcd under:

- `/global/mosdns/plugins`

Value must be a YAML list. Example:

```yaml
- tag: cache
  type: cache
  args:
    size: 4194304
    lazy_cache_ttl: 86400
    dump_file: cache.dat
    dump_interval: 600

- tag: hosts
  type: hosts
  args:
    files:
      - /etc/mosdns/hosts.txt
```

This list becomes the `plugins:` section of `/etc/mosdns/config.yaml`.
If the key is missing or empty, `/mosdns/config.yaml` is used as the default.

## Rule updates

Rules are refreshed based on the `refresh` interval only.
## Refresh

Rule updates are controlled by `/nodes/<NODE_ID>/mosdns/refresh` (minutes).
Default is `1440` (24 hours). If missing or invalid, the default is used.

## Socks port

MosDNS uses a fixed SOCKS port: `7891`.

## HTTP proxy for rule downloads

Rules are downloaded via HTTP proxy (default `http://127.0.0.1:7890`).
Override with `MOSDNS_HTTP_PROXY` if needed.
