#!/bin/bash
set -euo pipefail

NODE_ID="${1:-gw-a}"

etcdctl put /nodes/${NODE_ID}/router_id 10.42.1.1
etcdctl put /nodes/${NODE_ID}/roles/is_edge true

# Local LANs (sites merged into nodes 1:1)
etcdctl put /nodes/${NODE_ID}/lan $'10.42.10.0/24'

# EasyTier
etcdctl put /nodes/${NODE_ID}/easytier/enable true
etcdctl put /nodes/${NODE_ID}/easytier/dev_name et0
etcdctl put /nodes/${NODE_ID}/easytier/ipv4 10.42.1.1/24
etcdctl put /nodes/${NODE_ID}/easytier/listeners $'tcp:11010
udp:11011
tcp://0.0.0.0:11010'

# OSPF
etcdctl put /nodes/${NODE_ID}/ospf/enable true
etcdctl put /nodes/${NODE_ID}/ospf/area 0
etcdctl put /nodes/${NODE_ID}/ospf/active_ifaces $'et0'
etcdctl put /nodes/${NODE_ID}/ospf/inject_site_lan true
etcdctl put /nodes/${NODE_ID}/ospf/redistribute_bgp true

# BGP (edge)
etcdctl put /nodes/${NODE_ID}/bgp/enable true
etcdctl put /nodes/${NODE_ID}/bgp/local_asn 65010
etcdctl put /nodes/${NODE_ID}/bgp/max_paths 4
etcdctl put /nodes/${NODE_ID}/bgp/to_ospf/default_only true

# Clash
etcdctl put /nodes/${NODE_ID}/clash/enable true
etcdctl put /nodes/${NODE_ID}/clash/mode tproxy
etcdctl put /nodes/${NODE_ID}/clash/subscriptions/main/url "https://example.com/sub.yaml"
etcdctl put /nodes/${NODE_ID}/clash/active_subscription main
etcdctl put /nodes/${NODE_ID}/clash/refresh/enable true
etcdctl put /nodes/${NODE_ID}/clash/refresh/interval_minutes 60

etcdctl put /commit "$(date +%s)"

# OpenVPN (new schema)
etcdctl put /nodes/${NODE_ID}/openvpn/tun0/enable true
etcdctl put /nodes/${NODE_ID}/openvpn/tun0/dev tun0
etcdctl put /nodes/${NODE_ID}/openvpn/tun0/proto tcp-client
etcdctl put /nodes/${NODE_ID}/openvpn/tun0/port 1194
etcdctl put /nodes/${NODE_ID}/openvpn/tun0/remote "vpn.example.com"
etcdctl put /nodes/${NODE_ID}/openvpn/tun0/ifconfig "10.8.0.2 10.8.0.1"
etcdctl put /nodes/${NODE_ID}/openvpn/tun0/keepalive "10 60"
etcdctl put /nodes/${NODE_ID}/openvpn/tun0/cipher AES-256-CBC
etcdctl put /nodes/${NODE_ID}/openvpn/tun0/auth SHA256
etcdctl put /nodes/${NODE_ID}/openvpn/tun0/secret "INLINE_SECRET"
etcdctl put /nodes/${NODE_ID}/openvpn/tun0/bgp/peer_asn 65001
etcdctl put /nodes/${NODE_ID}/openvpn/tun0/bgp/peer_ip 10.8.0.1
etcdctl put /nodes/${NODE_ID}/openvpn/tun0/bgp/update_source tun0

# Global BGP filter policy (shared for all neighbors)
etcdctl put /global/bgp/filter/in  $'deny 0.0.0.0/0\npermit 0.0.0.0/0 le 32'
etcdctl put /global/bgp/filter/out $'permit 0.0.0.0/0 le 32'

# Global EasyTier identity
etcdctl put /global/mesh_type easytier
etcdctl put /global/easytier/network_name my-net
etcdctl put /global/easytier/network_secret my-secret
etcdctl put /global/easytier/private_mode true
etcdctl put /global/easytier/dhcp false

# Global Clash subscriptions
etcdctl put /global/clash/subscriptions/main/url "https://example.com/sub.yaml"

# EasyTier mapped listeners (optional)
# Public mapped address must correspond to a local listener port.
etcdctl put /nodes/${NODE_ID}/easytier/mapped_listeners $'tcp://203.0.113.10:443'

# ---- Tinc example (optional) ----
# Switch mesh to tinc:
# etcdctl put /global/mesh_type tinc
# etcdctl put /global/tinc/netname mesh
# etcdctl put /global/tinc/cipher blowfish
# etcdctl put /global/tinc/digest sha1
# etcdctl put /nodes/${NODE_ID}/tinc/enable true
# etcdctl put /nodes/${NODE_ID}/tinc/name ${NODE_ID}
# etcdctl put /nodes/${NODE_ID}/tinc/dev_name tnc0
# etcdctl put /nodes/${NODE_ID}/tinc/port 655
# etcdctl put /nodes/${NODE_ID}/tinc/address 203.0.113.10
# etcdctl put /nodes/${NODE_ID}/tinc/address_family ipv4
# etcdctl put /nodes/${NODE_ID}/tinc/ipv4 10.42.2.1/24
# etcdctl put /nodes/${NODE_ID}/tinc/subnet $'10.42.2.0/24'
# etcdctl put /nodes/${NODE_ID}/tinc/host_mode switch
# etcdctl put /nodes/${NODE_ID}/tinc/host_cipher blowfish
# etcdctl put /nodes/${NODE_ID}/tinc/host_digest sha1
# etcdctl put /nodes/${NODE_ID}/tinc/mode Switch
# etcdctl put /nodes/${NODE_ID}/tinc/public_key "$(cat ./tinc.pub)"
# etcdctl put /nodes/${NODE_ID}/tinc/ed25519_public_key "$(cat ./tinc.ed25519.pub)"
# etcdctl put /nodes/${NODE_ID}/tinc/private_key "$(cat ./tinc.priv)"
# etcdctl put /nodes/${NODE_ID}/tinc/ed25519_private_key "$(cat ./tinc.ed25519.priv)"
