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
	MEDUZA_POLL=5
	MEDUZA_REPORT=15
	MEDUZA_TTL=60
	MEDUZA_TIMEOUT=10
	[ -n "$MEDUZA_NODE" ] || { log 'UCI option NODE_ID is required'; return 1; }
	MEDUZA_ENDPOINT=${MEDUZA_ENDPOINTS%%,*}
	[ -n "$MEDUZA_ENDPOINT" ] || { log 'UCI option ETCD_ENDPOINTS is required'; return 1; }
	MEDUZA_ENDPOINT=${MEDUZA_ENDPOINT%/}
	mkdir -p "$MEDUZA_STATE"
}

b64e() { printf '%s' "$1" | base64 | tr -d '\n'; }
b64d() { printf '%s' "$1" | base64 -d; }

curl_args() {
	printf '%s\n' --silent --show-error --fail --max-time "$MEDUZA_TIMEOUT"
	[ -n "$MEDUZA_CA" ] && printf '%s\n' --cacert "$MEDUZA_CA"
	[ -n "$MEDUZA_CERT" ] && printf '%s\n' --cert "$MEDUZA_CERT"
	[ -n "$MEDUZA_KEY" ] && printf '%s\n' --key "$MEDUZA_KEY"
}

etcd_token() {
	[ -n "$MEDUZA_USER" ] || return 0
	local body
	body=$(jq -nc --arg n "$MEDUZA_USER" --arg p "$MEDUZA_PASS" '{name:$n,password:$p}')
	curl $(curl_args) -H 'Content-Type: application/json' -d "$body" \
		"$MEDUZA_ENDPOINT/v3/auth/authenticate" | jq -r '.token // empty'
}

etcd_post() {
	local path=$1 body=$2 token=${ETCD_TOKEN:-}
	[ -z "$token" ] && token=$(etcd_token) && ETCD_TOKEN=$token
	if [ -n "$token" ]; then
		curl $(curl_args) -H 'Content-Type: application/json' -H "Authorization: $token" -d "$body" "$MEDUZA_ENDPOINT$path"
	else
		curl $(curl_args) -H 'Content-Type: application/json' -d "$body" "$MEDUZA_ENDPOINT$path"
	fi
}

prefix_end() {
	# All Meduza prefixes are ASCII and end in '/'; increment the final slash.
	printf '%s0' "${1%/}"
}

etcd_range() {
	local prefix=$1 end body
	end=$(prefix_end "$prefix")
	body=$(jq -nc --arg k "$(b64e "$prefix")" --arg e "$(b64e "$end")" '{key:$k,range_end:$e}')
	etcd_post /v3/kv/range "$body" | jq -c '[.kvs[]? | {key:(.key|@base64d),value:(.value|@base64d)}] | from_entries'
}

etcd_get() {
	local key=$1 body
	body=$(jq -nc --arg k "$(b64e "$key")" '{key:$k}')
	etcd_post /v3/kv/range "$body" | jq -r '.kvs[0].value // empty | @base64d'
}

etcd_put() {
	local key=$1 value=$2 lease=${3:-0} body
	body=$(jq -nc --arg k "$(b64e "$key")" --arg v "$(b64e "$value")" --argjson l "$lease" \
		'{key:$k,value:$v} + (if $l > 0 then {lease:$l} else {} end)')
	etcd_post /v3/kv/put "$body" >/dev/null
}

etcd_lease() {
	local body lease_id
	lease_id=$(( $(date +%s) * 1000 + $$ % 1000 ))
	body=$(jq -nc --argjson t "$1" --argjson id "$lease_id" '{TTL:$t,ID:$id}')
	etcd_post /v3/lease/grant "$body" | jq -r '.ID // 0'
}

utc_now() { date -u '+%Y-%m-%dT%H:%M:%S+0000'; }

atomic_write() {
	local target=$1 mode=${2:-600} tmp="${target}.meduza.$$"
	mkdir -p "${target%/*}"
	cat >"$tmp" && chmod "$mode" "$tmp" && mv "$tmp" "$target"
}
