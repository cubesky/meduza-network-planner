#!/bin/sh

MEDUZA_STATE=${MEDUZA_STATE:-/var/run/meduza}
MEDUZA_CONFIG=${MEDUZA_CONFIG:-/etc/config/meduza}

log() { logger -t meduza "$*"; echo "meduza: $*" >&2; }

load_settings() {
	config_load meduza
	config_get MEDUZA_NODE main NODE_ID
	config_get MEDUZA_ENDPOINTS main ETCD_ENDPOINTS 'https://127.0.0.1:2379'
	config_get MEDUZA_CA main ETCD_CA
	config_get MEDUZA_CERT main ETCD_CERT
	config_get MEDUZA_KEY main ETCD_KEY
	config_get MEDUZA_USER main ETCD_USER
	config_get MEDUZA_PASS main ETCD_PASS
	[ -n "$MEDUZA_NODE" ] || { log 'UCI option NODE_ID is required'; return 1; }
	mkdir -p "$MEDUZA_STATE"
}

atomic_write() {
	local target=$1 mode=${2:-600} tmp="${target}.meduza.$$"
	mkdir -p "${target%/*}"
	cat >"$tmp" && chmod "$mode" "$tmp" && mv "$tmp" "$target"
}
