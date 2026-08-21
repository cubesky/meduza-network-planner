#!/bin/sh

MEDUZA_STATE=${MEDUZA_STATE:-/var/run/meduza}
MEDUZA_CONFIG=${MEDUZA_CONFIG:-/etc/config/meduza}
MEDUZA_DATA=${MEDUZA_DATA:-/etc/meduza}
MEDUZA_GENERATED=${MEDUZA_GENERATED:-$MEDUZA_DATA/generated}
MEDUZA_MANAGED=${MEDUZA_MANAGED:-$MEDUZA_DATA/managed/interfaces}
MEDUZA_PENDING=${MEDUZA_PENDING:-$MEDUZA_DATA/managed/interfaces.pending}
MEDUZA_LEGACY_AUTH=${MEDUZA_LEGACY_AUTH:-$MEDUZA_DATA/managed/legacy.interfaces}
MEDUZA_OWNER=meduza-openwrt-lite

log() { logger -t meduza "$*"; echo "meduza: $*" >&2; }

durable_mkdir() {
	local path=$1 mode=${2:-700} parent
	parent=${path%/*}; [ -n "$parent" ] || parent=/
	[ ! -L "$path" ] || return 1
	if [ ! -d "$path" ]; then
		mkdir "$path" || return 1
	fi
	chmod "$mode" "$path" || return 1
	# Also fsync the already-present case: it may be a retry after mkdir
	# succeeded but the parent-directory fsync was interrupted by power loss.
	fsync "$path" || return 1
	fsync "$parent" || return 1
}

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
	valid_node_id "$MEDUZA_NODE" || { log "invalid UCI NODE_ID: $MEDUZA_NODE"; return 1; }
	umask 077
	for path in "$MEDUZA_DATA" "$MEDUZA_DATA/managed" "$MEDUZA_GENERATED"; do
		[ ! -L "$path" ] || { log "refusing symlinked Meduza data path: $path"; return 1; }
	done
	mkdir -p "$MEDUZA_STATE" || return 1
	chmod 700 "$MEDUZA_STATE" || return 1
	durable_mkdir "$MEDUZA_DATA" 700 || return 1
	durable_mkdir "$MEDUZA_DATA/managed" 700 || return 1
	durable_mkdir "$MEDUZA_GENERATED" 700 || return 1
}

# Write stdin atomically. When the contents are unchanged the target is left
# untouched, allowing callers to avoid needless VPN and OpenClash reloads.
# An optional third argument is touched only when the target actually changes.
atomic_write() {
	local target mode changed tmp
	target=$1
	mode=${2:-600}
	changed=${3:-}
	tmp="${target}.meduza.$$"
	mkdir -p "${target%/*}" || return 1
	cat >"$tmp" || { rm -f "$tmp"; return 1; }
	chmod "$mode" "$tmp" || { rm -f "$tmp"; return 1; }
	fsync "$tmp" || { rm -f "$tmp"; return 1; }
	if [ -f "$target" ] && cmp -s "$tmp" "$target"; then
		rm -f "$tmp" || return 1
		fsync "${target%/*}" || return 1
		return 0
	fi
	if [ -n "$changed" ]; then
		: >"$changed" || { rm -f "$tmp"; return 1; }
		fsync "${changed%/*}" || { rm -f "$tmp"; return 1; }
	fi
	mv "$tmp" "$target" || { rm -f "$tmp"; return 1; }
	fsync "${target%/*}" || return 1
}

valid_instance_name() {
	case "$1" in
		''|.|..|-*|*[!A-Za-z0-9_-]*) return 1 ;;
	esac
	[ "${#1}" -le 64 ]
}

valid_node_id() {
	case "$1" in
		''|[!A-Za-z0-9_]*|*[!A-Za-z0-9_.-]*) return 1 ;;
	esac
	[ "${#1}" -le 128 ]
}

valid_uci_name() {
	case "$1" in ''|*[!A-Za-z0-9_]*) return 1;; esac
}

valid_device_name() {
	case "$1" in
		''|-*|lo|utun|*[!A-Za-z0-9_.-]*) return 1 ;;
	esac
	# Linux IFNAMSIZ includes the terminating NUL.
	[ "${#1}" -le 15 ]
}

uci_instance_name() {
	printf '%s' "$1" | tr '-' '_'
}

valid_managed_entry() {
	local kind=$1 instance=$2 logical=$3 device=$4 config=$5 expected
	valid_instance_name "$instance" || return 1
	valid_uci_name "$logical" || return 1
	valid_device_name "$device" || return 1
	case "$kind" in
		tinc) expected="tinc_$(uci_instance_name "$instance")" ;;
		openvpn) expected="ovpn_$(uci_instance_name "$instance")" ;;
		wireguard) expected="wg_$(uci_instance_name "$instance")" ;;
		*) return 1 ;;
	esac
	[ "$logical" = "$expected" ] || return 1
	case "$kind:$config" in
		"tinc:$MEDUZA_GENERATED/tinc/$instance/tinc.conf"|\
		"openvpn:$MEDUZA_GENERATED/openvpn/$instance/openvpn.conf"|\
		"wireguard:$MEDUZA_GENERATED/wireguard/$instance/wg.conf"|\
		"tinc:/etc/tinc/$instance/tinc.conf"|\
		"openvpn:/etc/openvpn/meduza-$instance.conf"|\
		"wireguard:$MEDUZA_DATA/wireguard/$instance.conf") return 0 ;;
		*) return 1 ;;
	esac
}

default_device_name() {
	local kind name candidate prefix keep checksum short
	kind=$1
	name=$2
	case "$kind" in
		openvpn) prefix=ovpn; keep=5 ;;
		wireguard) prefix=wg; keep=7 ;;
		*) return 1 ;;
	esac
	candidate="$prefix-$name"
	if [ "${#candidate}" -le 15 ]; then
		printf '%s' "$candidate"
		return 0
	fi
	checksum=$(python3 -c \
		'import hashlib,sys; print(hashlib.sha256(sys.argv[1].encode()).hexdigest()[:4])' \
		"$kind:$name")
	short=$(printf '%s' "$name" | cut -c "1-$keep")
	printf '%s-%s-%s' "$prefix" "$short" "$checksum"
}

manifest_has_device() {
	local kind=$1 instance=$2 device=$3 file=${4:-$MEDUZA_STATE/inventory.interfaces}
	[ -f "$file" ] || return 1
	awk -F '\t' -v k="$kind" -v i="$instance" -v d="$device" \
		'$1 == k && $2 == i && $4 == d { found=1 } END { exit !found }' "$file"
}

runtime_device_has() {
	local kind=$1 instance=$2 device=$3 file=$MEDUZA_STATE/runtime.devices
	[ -f "$file" ] || return 1
	awk -F '\t' -v k="$kind" -v i="$instance" -v d="$device" \
		'$1 == k && $2 == i && $3 == d { found=1 } END { exit !found }' "$file"
}

runtime_device_add() {
	local kind=$1 instance=$2 device=$3 file=$MEDUZA_STATE/runtime.devices
	runtime_device_has "$kind" "$instance" "$device" && return 0
	printf '%s\t%s\t%s\n' "$kind" "$instance" "$device" >>"$file"
	chmod 600 "$file"
}

runtime_device_remove() {
	local kind=$1 instance=$2 device=$3 file=$MEDUZA_STATE/runtime.devices tmp
	[ -f "$file" ] || return 0
	tmp="$file.tmp.$$"
	awk -F '\t' -v k="$kind" -v i="$instance" -v d="$device" \
		'!($1 == k && $2 == i && $3 == d)' "$file" >"$tmp"
	mv "$tmp" "$file"
}

managed_device_alias() {
	printf '%s:%s:%s' "$MEDUZA_OWNER" "$1" "$2"
}

device_is_owned() {
	local kind=$1 instance=$2 device=$3 alias
	valid_device_name "$device" || return 1
	[ -r "/sys/class/net/$device/ifalias" ] || return 1
	alias=$(cat "/sys/class/net/$device/ifalias" 2>/dev/null || true)
	[ "$alias" = "$(managed_device_alias "$kind" "$instance")" ]
}

mark_device_owned() {
	local kind=$1 instance=$2 device=$3 alias
	valid_device_name "$device" || return 1
	[ -e "/sys/class/net/$device" ] || return 1
	alias=$(managed_device_alias "$kind" "$instance")
	ip link set dev "$device" alias "$alias" >/dev/null 2>&1 || \
		printf '%s' "$alias" >"/sys/class/net/$device/ifalias" 2>/dev/null || return 1
	device_is_owned "$kind" "$instance" "$device"
}

acquire_generator_lock() {
	local base pid command_line alive attempts=0 current token
	base=/var/lock
	[ -d "$base" ] || base=$MEDUZA_STATE
	MEDUZA_LOCK="$base/meduza-generator.lock"
	token="$$:$(python3 -c 'import secrets; print(secrets.token_hex(8))')" || return 1
	MEDUZA_LOCK_TOKEN=$token
	while ! ln -s "$token" "$MEDUZA_LOCK" 2>/dev/null; do
		# Migrate a stale directory lock from the previous package revision.
		if [ -d "$MEDUZA_LOCK" ] && [ ! -L "$MEDUZA_LOCK" ]; then
			pid=$(cat "$MEDUZA_LOCK/pid" 2>/dev/null || true)
			if [ -z "$pid" ]; then
				# Never reclaim the old mkdir->pid initialization window.  Both
				# candidate lock roots are tmpfs and reboot clears it safely.
				alive=1
			else
				alive=0
				if kill -0 "$pid" 2>/dev/null && [ -r "/proc/$pid/cmdline" ]; then
					command_line=$(tr '\000' ' ' <"/proc/$pid/cmdline" 2>/dev/null || true)
					case "$command_line" in *meduza-generator*) alive=1;; esac
				fi
				if [ "$alive" = 0 ] && [ "$(cat "$MEDUZA_LOCK/pid" 2>/dev/null || true)" = "$pid" ]; then
					rm -f "$MEDUZA_LOCK/pid" 2>/dev/null || true
					rmdir "$MEDUZA_LOCK" 2>/dev/null || true
				fi
			fi
		elif [ -L "$MEDUZA_LOCK" ]; then
			current=$(readlink "$MEDUZA_LOCK" 2>/dev/null || true)
			pid=${current%%:*}
			alive=0
			case "$pid" in ''|*[!0-9]*) :;;
				*)
					if kill -0 "$pid" 2>/dev/null && [ -r "/proc/$pid/cmdline" ]; then
						command_line=$(tr '\000' ' ' <"/proc/$pid/cmdline" 2>/dev/null || true)
						case "$command_line" in *meduza-generator*) alive=1;; esac
					fi
					;;
			esac
			if [ "$alive" = 0 ] && [ "$(readlink "$MEDUZA_LOCK" 2>/dev/null || true)" = "$current" ]; then
				rm -f "$MEDUZA_LOCK" 2>/dev/null || true
			fi
		else
			log "generator lock path has an unsafe type: $MEDUZA_LOCK"
			return 1
		fi
		attempts=$((attempts + 1))
		[ "$attempts" -lt 30 ] || { log 'timed out waiting for generator lock'; return 1; }
		sleep 1
	done
	trap 'release_generator_lock' EXIT
	trap 'release_generator_lock; exit 130' INT
	trap 'release_generator_lock; exit 143' TERM
}

release_generator_lock() {
	[ -n "${MEDUZA_LOCK:-}" ] || return 0
	if [ -L "$MEDUZA_LOCK" ] && \
		[ "$(readlink "$MEDUZA_LOCK" 2>/dev/null || true)" = "${MEDUZA_LOCK_TOKEN:-}" ]; then
		rm -f "$MEDUZA_LOCK" 2>/dev/null || true
	fi
	MEDUZA_LOCK=
	MEDUZA_LOCK_TOKEN=
}
