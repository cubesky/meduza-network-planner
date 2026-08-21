include $(TOPDIR)/rules.mk

PKG_NAME:=meduza-openwrt-lite
PKG_VERSION:=0.1.0
PKG_RELEASE:=27
MEDUZA_BUILD_ID:=$(PKG_VERSION)-r$(PKG_RELEASE)

MEDUZA_PKGARCH?=$(ARCH_PACKAGES)

include $(INCLUDE_DIR)/package.mk

define Package/meduza-openwrt-lite
  SECTION:=net
  CATEGORY:=Network
  TITLE:=Lightweight Meduza etcd reconciler for OpenWrt
  PKGARCH:=all
  DEPENDS:=+python3 +rpcd +python3-etcd3 +python3-grpcio +python3-protobuf \
    +python3-six +python3-tenacity +python3-typing-extensions
endef

define Package/python3-etcd3
  SECTION:=lang
  CATEGORY:=Languages
  SUBMENU:=Python
  TITLE:=Python etcd v3 client for Meduza
  PKGARCH:=all
  DEPENDS:=+python3 +python3-grpcio +python3-protobuf +python3-six \
    +python3-tenacity
endef

define Package/python3-grpcio
  SECTION:=lang
  CATEGORY:=Languages
  SUBMENU:=Python
  TITLE:=Python gRPC runtime for Meduza
  PKGARCH:=$(MEDUZA_PKGARCH)
  DEPENDS:=+python3 +libstdcpp6
endef

define Package/python3-protobuf
  SECTION:=lang
  CATEGORY:=Languages
  SUBMENU:=Python
  TITLE:=Python Protocol Buffers runtime for Meduza
  PKGARCH:=all
  DEPENDS:=+python3
endef

define Package/python3-six
  SECTION:=lang
  CATEGORY:=Languages
  SUBMENU:=Python
  TITLE:=Python 2 and 3 compatibility helpers for Meduza
  PKGARCH:=all
  DEPENDS:=+python3
endef

define Package/python3-typing-extensions
  SECTION:=lang
  CATEGORY:=Languages
  SUBMENU:=Python
  TITLE:=Backported Python typing extensions for Meduza
  PKGARCH:=all
  DEPENDS:=+python3
endef

define Package/python3-tenacity
  SECTION:=lang
  CATEGORY:=Languages
  SUBMENU:=Python
  TITLE:=Python retrying library for Meduza
  PKGARCH:=all
  DEPENDS:=+python3
endef

define Package/meduza-openwrt-lite/description
 Pulls Meduza's declarative configuration from etcd and configures the native
 OpenWrt tinc, FRR, OpenVPN and WireGuard packages. Reports node and tunnel
 health back to etcd through a resilient Python agent without a container runtime.
endef

define Package/meduza-openwrt-lite/conffiles
/etc/config/meduza
endef

define Package/meduza-openwrt-lite/prerm
#!/bin/sh
[ -n "$${IPKG_INSTROOT:-}" ] && exit 0
if [ "$${PKG_UPGRADE:-0}" != 1 ] && [ -x /usr/libexec/meduza/meduza-recover ]; then
	/usr/libexec/meduza/meduza-recover --install-bundle >/dev/null 2>&1 || exit 1
fi
if [ -x /etc/init.d/meduza ]; then
	/etc/init.d/meduza stop >/dev/null 2>&1 || exit 1
fi
[ "$${PKG_UPGRADE:-0}" = 1 ] && exit 0
[ ! -x /usr/libexec/meduza/meduza-generator ] || \
	/usr/libexec/meduza/meduza-generator --purge >/dev/null 2>&1 || exit 1
[ ! -L /etc/meduza/managed/install-complete ] || exit 1
rm -f /etc/meduza/managed/install-complete || exit 1
for temporary in /etc/meduza/managed/install-complete.meduza.[0-9]*; do
	[ -e "$${temporary}" ] || continue
	[ -f "$${temporary}" ] && [ ! -L "$${temporary}" ] || exit 1
	rm -f "$${temporary}" || exit 1
done
fsync /etc/meduza/managed || exit 1
[ ! -x /usr/libexec/meduza/meduza-recover ] || \
	/usr/libexec/meduza/meduza-recover --remove-bundle >/dev/null 2>&1 || exit 1
[ ! -x /etc/init.d/meduza ] || /etc/init.d/meduza disable >/dev/null 2>&1 || true
exit 0
endef

define Package/meduza-openwrt-lite/postinst
#!/bin/sh
[ -n "$${IPKG_INSTROOT:-}" ] && exit 0
[ -f /usr/share/meduza/openwrt-lite-build ] && \
	[ ! -L /usr/share/meduza/openwrt-lite-build ] || exit 1
build_id=$$(cat /usr/share/meduza/openwrt-lite-build 2>/dev/null) || exit 1
case "$${build_id}" in *[!A-Za-z0-9_.-]*|'') exit 1;; esac
[ "$${build_id}" = '$(MEDUZA_BUILD_ID)' ] || exit 1
state=/etc/meduza/managed/upgrade.state
complete=/etc/meduza/managed/install-complete
first_install=/etc/meduza/managed/upgrade.first-install
if [ ! -e "$${state}" ] && [ ! -L "$${state}" ]; then
	# A repeated configure after the successful handoff is idempotent.  It may
	# never recreate authority from an incomplete or mismatched completion seal.
	[ -f "$${complete}" ] && [ ! -L "$${complete}" ] || exit 1
	line=$$(cat "$${complete}" 2>/dev/null) || exit 1
	old_ifs=$${IFS}; IFS=$$(printf '\t'); set -- $${line}; IFS=$${old_ifs}
	[ "$$#" -eq 3 ] && [ "$$1" = v1 ] && [ "$$2" = "$${build_id}" ] || exit 1
	case "$$3" in *[!0-9a-f]*|'') exit 1;; esac
	[ "$${#3}" -eq 32 ] || exit 1
	transaction=$$3
	[ ! -e /etc/meduza/managed/upgrade.rc-link ] && \
		[ ! -L /etc/meduza/managed/upgrade.rc-link ] && \
		[ ! -e /etc/meduza/managed/upgrade.rc-disabled ] || exit 1
	if [ -e "$${first_install}" ]; then
		[ -f "$${first_install}" ] && [ ! -L "$${first_install}" ] && \
			[ "$$(cat "$${first_install}" 2>/dev/null)" = "v1:$${transaction}:$${build_id}" ] || exit 1
		rm -f "$${first_install}" || exit 1
		fsync /etc/meduza/managed || exit 1
	fi
	exit 0
fi
[ -f "$${state}" ] && [ ! -L "$${state}" ] || exit 1
state_line=$$(cat "$${state}" 2>/dev/null) || exit 1
old_ifs=$${IFS}; IFS=:; set -- $${state_line}; IFS=$${old_ifs}
[ "$$#" -eq 3 ] && [ "$$1" = ready ] && [ "$$3" = "$${build_id}" ] || exit 1
transaction=$$2
case "$${transaction}" in *[!0-9a-f]*|'') exit 1;; esac
[ "$${#transaction}" -eq 32 ] || exit 1
[ ! -x /usr/libexec/meduza/meduza-recover ] || \
	/usr/libexec/meduza/meduza-recover --install-bundle >/dev/null 2>&1 || exit 1
# Restore the original boot entry before publishing the completion seal.  If
# power fails between these two durable steps the new init is reachable but its
# seal gate still refuses all mutation; after the seal lands, boot can finish
# the handoff even when this postinst never reaches the explicit start below.
rc_link=/etc/rc.d/S95meduza
saved_rc=/etc/meduza/managed/upgrade.rc-link
rc_disabled=/etc/meduza/managed/upgrade.rc-disabled
[ -d /etc/rc.d ] && [ ! -L /etc/rc.d ] || exit 1
if [ -e "$${saved_rc}" ] || [ -L "$${saved_rc}" ]; then
	[ -L "$${saved_rc}" ] || exit 1
	target=$$(readlink "$${saved_rc}") || exit 1
	case "$${target}" in ../init.d/meduza|/etc/init.d/meduza) :;; *) exit 1;; esac
	[ ! -e "$${rc_disabled}" ] && [ ! -L "$${rc_disabled}" ] || exit 1
	if [ -e "$${rc_link}" ] || [ -L "$${rc_link}" ]; then
		[ -L "$${rc_link}" ] && [ "$$(readlink "$${rc_link}")" = "$${target}" ] || exit 1
	else
		ln -s "$${target}" "$${rc_link}" || exit 1
	fi
	fsync /etc/rc.d || exit 1
elif [ -e "$${rc_disabled}" ]; then
	[ -f "$${rc_disabled}" ] && [ ! -L "$${rc_disabled}" ] && \
		[ "$$(cat "$${rc_disabled}" 2>/dev/null)" = disabled ] || exit 1
	[ ! -e "$${rc_link}" ] && [ ! -L "$${rc_link}" ] || exit 1
else
	exit 1
fi
fsync /etc/meduza/managed || exit 1
for temporary in "$${complete}.meduza."[0-9]*; do
	[ -e "$${temporary}" ] || continue
	[ -f "$${temporary}" ] && [ ! -L "$${temporary}" ] || exit 1
	rm -f "$${temporary}" || exit 1
done
tmp=$${complete}.meduza.$$$$
printf 'v1\t%s\t%s\n' "$${build_id}" "$${transaction}" >"$${tmp}" || exit 1
chmod 600 "$${tmp}" || exit 1
fsync "$${tmp}" || exit 1
mv -f "$${tmp}" "$${complete}" || exit 1
fsync /etc/meduza/managed || exit 1
if [ -e "$${first_install}" ]; then
	[ -f "$${first_install}" ] && [ ! -L "$${first_install}" ] && \
		[ "$$(cat "$${first_install}" 2>/dev/null)" = "v1:$${transaction}:$${build_id}" ] || exit 1
	rm -f "$${first_install}" || exit 1
	fsync /etc/meduza/managed || exit 1
fi
/etc/init.d/meduza start || exit 1
exit 0
endef

define Package/meduza-openwrt-lite/preinst
#!/bin/sh
[ -n "$${IPKG_INSTROOT:-}" ] && exit 0
# APK runs this as pre-upgrade but does not abort the file transaction when a
# package script fails.  Journal the unsafe state before doing anything else;
# the newly unpacked init script refuses to start until this exact old service
# has stopped and the one-shot migration snapshot is durable.
umask 077
set -f
[ ! -L /etc/meduza ] && [ ! -L /etc/meduza/managed ] || exit 1
mkdir -p /etc/meduza/managed || exit 1

managed=/etc/meduza/managed
upgrade_state=$${managed}/upgrade.state
upgrade_intent=$${managed}/upgrade.intent
migration=$${managed}/uci-migration.zone
seal=$${managed}/uci-migration.zone.seal
disabled=$${managed}/uci-migration.zone.disabled
rc_link=/etc/rc.d/S95meduza
saved_rc=$${managed}/upgrade.rc-link
rc_disabled=$${managed}/upgrade.rc-disabled
first_install=$${managed}/upgrade.first-install

meduza_atomic_value() {
	local value="$$1" target="$$2" tmp="$${2}.meduza.$$$$"
	[ ! -L "$${target}" ] || return 1
	rm -f "$${tmp}" || return 1
	printf '%s\n' "$${value}" >"$${tmp}" || return 1
	chmod 600 "$${tmp}" || return 1
	fsync "$${tmp}" || return 1
	mv -f "$${tmp}" "$${target}" || return 1
	fsync "$${managed}" || return 1
}

meduza_install_file() {
	local source="$$1" target="$$2"
	[ -f "$${source}" ] && [ ! -L "$${source}" ] && [ ! -L "$${target}" ] || return 1
	chmod 600 "$${source}" || return 1
	fsync "$${source}" || return 1
	mv -f "$${source}" "$${target}" || return 1
	fsync "$${managed}" || return 1
}

meduza_file_hash() {
	python3 -c 'import hashlib, os, stat, sys
path = sys.argv[1]
info = os.lstat(path)
if stat.S_ISLNK(info.st_mode) or not stat.S_ISREG(info.st_mode):
    raise SystemExit(2)
digest = hashlib.sha256()
with open(path, "rb") as handle:
    for block in iter(lambda: handle.read(65536), b""):
        digest.update(block)
print(digest.hexdigest())' "$$1"
}

meduza_upgrade_input_hash() {
	python3 -c 'import hashlib, os, stat, sys
digest = hashlib.sha256()
for path in sys.argv[1:]:
    digest.update(path.encode("utf-8") + b"\0")
    try:
        info = os.lstat(path)
    except FileNotFoundError:
        digest.update(b"missing\0")
        continue
    if stat.S_ISLNK(info.st_mode) or not stat.S_ISREG(info.st_mode):
        raise SystemExit(2)
    digest.update(b"file\0")
    with open(path, "rb") as handle:
        for block in iter(lambda: handle.read(65536), b""):
            digest.update(block)
    digest.update(b"\0")
print(digest.hexdigest())' \
		/etc/config/meduza /etc/config/firewall /etc/config/network \
		/etc/config/openvpn "$${managed}/interfaces" \
		"$${managed}/interfaces.pending" \
		/usr/libexec/meduza/meduza-generator
}

meduza_agent_registered() {
	local snapshot=/var/run/meduza/preupgrade-procd.$$$$ status
	mkdir -p /var/run/meduza || return 2
	ubus -S call service list '{"name":"meduza"}' >"$${snapshot}" 2>/dev/null || return 2
	python3 -c 'import json, sys
try:
    value = json.load(open(sys.argv[1], encoding="utf-8"))
    service = value.get("meduza")
    instances = service.get("instances", {}) if isinstance(service, dict) else {}
    agent = instances.get("agent") if isinstance(instances, dict) else None
    command = agent.get("command") if isinstance(agent, dict) else None
except (OSError, ValueError, AttributeError):
    raise SystemExit(2)
expected = ["/usr/bin/python3", "/usr/libexec/meduza/meduza-agent.py"]
if command == expected:
    raise SystemExit(0)
if not instances:
    raise SystemExit(1)
raise SystemExit(2)' "$${snapshot}"
	status=$$?
	rm -f "$${snapshot}" || return 2
	return "$${status}"
}

meduza_process_live() {
	python3 -c 'import os, sys
scripts = {
    "/usr/libexec/meduza/meduza-agent.py": "python",
    "/usr/libexec/meduza/meduza-uci-session.py": "python",
    "/usr/libexec/meduza/meduza-generator": "shell",
    "/usr/libexec/meduza/meduza-openwrt-sync": "shell",
}
found = False
for name in os.listdir("/proc"):
    if not name.isdigit():
        continue
    try:
        argv = open("/proc/" + name + "/cmdline", "rb").read().rstrip(b"\0").split(b"\0")
        argv = [item.decode(errors="surrogateescape") for item in argv]
    except OSError:
        continue
    if len(argv) < 2 or argv[1] not in scripts:
        continue
    executable = os.path.basename(argv[0])
    kind = scripts[argv[1]]
    if (kind == "python" and executable.startswith("python3")) or (
        kind == "shell" and executable in ("sh", "ash")
    ):
        found = True
        break
raise SystemExit(0 if found else 1)'
}

meduza_signal_processes() {
	python3 -c 'import os, signal, sys
scripts = {
    "/usr/libexec/meduza/meduza-agent.py": "python",
    "/usr/libexec/meduza/meduza-uci-session.py": "python",
    "/usr/libexec/meduza/meduza-generator": "shell",
    "/usr/libexec/meduza/meduza-openwrt-sync": "shell",
}
sig = getattr(signal, "SIG" + sys.argv[1])
for name in os.listdir("/proc"):
    if not name.isdigit() or int(name) == os.getpid():
        continue
    try:
        argv = open("/proc/" + name + "/cmdline", "rb").read().rstrip(b"\0").split(b"\0")
        argv = [item.decode(errors="surrogateescape") for item in argv]
    except OSError:
        continue
    if len(argv) < 2 or argv[1] not in scripts:
        continue
    executable = os.path.basename(argv[0])
    kind = scripts[argv[1]]
    if not ((kind == "python" and executable.startswith("python3")) or (
        kind == "shell" and executable in ("sh", "ash")
    )):
        continue
    try:
        os.kill(int(name), sig)
    except (ProcessLookupError, PermissionError):
        pass' "$$1"
}

# `default_postinst` starts the new init script before this package's custom
# postinst runs.  Bind that start to this exact package transaction, not merely
# to the release string: a same-version reinstall must not reuse an old seal.
new_build='$(MEDUZA_BUILD_ID)'
had_state=0
state_tx=
state_build=
if [ -e "$${upgrade_state}" ]; then
	[ -f "$${upgrade_state}" ] && [ ! -L "$${upgrade_state}" ] || exit 1
	state_line=$$(cat "$${upgrade_state}" 2>/dev/null) || exit 1
	old_ifs=$${IFS}; IFS=:; set -- $${state_line}; IFS=$${old_ifs}
	[ "$$#" -eq 3 ] || exit 1
	case "$$1" in blocked|ready) :;; *) exit 1;; esac
	state_tx=$$2
	state_build=$$3
	case "$${state_tx}" in *[!0-9a-f]*|'') exit 1;; esac
	[ "$${#state_tx}" -eq 32 ] || exit 1
	case "$${state_build}" in ''|*[!A-Za-z0-9_.-]*) exit 1;; esac
	had_state=1
fi

for temporary in "$${first_install}.meduza."[0-9]*; do
	[ -e "$${temporary}" ] || continue
	[ -f "$${temporary}" ] && [ ! -L "$${temporary}" ] || exit 1
	rm -f "$${temporary}" || exit 1
done
fsync "$${managed}" || exit 1
first_install_replay=0
first_install_terminal=0
if [ -e "$${first_install}" ]; then
	[ -f "$${first_install}" ] && [ ! -L "$${first_install}" ] || exit 1
	first_line=$$(cat "$${first_install}" 2>/dev/null) || exit 1
	old_ifs=$${IFS}; IFS=:; set -- $${first_line}; IFS=$${old_ifs}
	[ "$$#" -eq 3 ] && [ "$$1" = v1 ] || exit 1
	first_tx=$$2
	first_build=$$3
	case "$${first_tx}" in *[!0-9a-f]*|'') exit 1;; esac
	[ "$${#first_tx}" -eq 32 ] || exit 1
	case "$${first_build}" in ''|*[!A-Za-z0-9_.-]*) exit 1;; esac
	if [ -e "$${managed}/install-complete" ]; then
		[ -f "$${managed}/install-complete" ] && \
			[ ! -L "$${managed}/install-complete" ] || exit 1
		complete_line=$$(cat "$${managed}/install-complete" 2>/dev/null) || exit 1
		old_ifs=$${IFS}; IFS=$$(printf '\t'); set -- $${complete_line}; IFS=$${old_ifs}
		[ "$$#" -eq 3 ] && [ "$$1" = v1 ] && \
			[ "$$2" = "$${first_build}" ] && [ "$$3" = "$${first_tx}" ] || exit 1
		first_install_terminal=1
	else
		[ ! -L "$${managed}/install-complete" ] || exit 1
		first_install_replay=1
	fi
fi

# A first-install marker is either an interrupted transaction (no completion)
# or a terminal marker whose final unlink was interrupted.  In both cases this
# package invocation gets a new nonce.  The former remains a first-install
# replay; the latter is a normal reinstall/upgrade of an already sealed payload.
if [ "$${first_install_replay}" -eq 1 ] || [ "$${first_install_terminal}" -eq 1 ]; then
	tx_nonce=$$(python3 -c 'import secrets; print(secrets.token_hex(16))') || exit 1
elif [ "$${had_state}" -eq 1 ] && [ "$${state_build}" = "$${new_build}" ]; then
	tx_nonce=$${state_tx}
else
	tx_nonce=$$(python3 -c 'import secrets; print(secrets.token_hex(16))') || exit 1
fi
meduza_atomic_value "blocked:$${tx_nonce}:$${new_build}" "$${upgrade_state}" || exit 1
if [ "$${first_install_terminal}" -eq 1 ]; then
	rm -f "$${first_install}" || exit 1
	fsync "$${managed}" || exit 1
elif [ "$${first_install_replay}" -eq 1 ]; then
	meduza_atomic_value "v1:$${tx_nonce}:$${new_build}" "$${first_install}" || exit 1
fi

old_present=0
for path in /etc/init.d/meduza /usr/libexec/meduza/meduza-agent.py \
	/usr/libexec/meduza/meduza-generator "$${managed}/interfaces" \
	"$${managed}/interfaces.pending" "$${managed}/uci-ownership.json" \
	"$${managed}/recovery.bundle" "$${managed}/install-complete" \
	"$${managed}/upgrade.intent" "$${managed}/upgrade.rc-link" \
	"$${managed}/upgrade.rc-disabled" /var/run/meduza; do
	[ ! -e "$${path}" ] || old_present=1
done
if [ "$${old_present}" -eq 0 ] || [ "$${first_install_replay}" -eq 1 ]; then
	# First install: default_postinst may try to start before this package's
	# custom postinst publishes install-complete.  Give that later handoff the
	# same ready/rc journal as an upgrade, without inventing migration authority.
	# The first-install phase makes a pre-payload power-loss replay unambiguous.
	if [ "$${first_install_replay}" -eq 0 ]; then
		meduza_atomic_value "v1:$${tx_nonce}:$${new_build}" "$${first_install}" || exit 1
	fi
	if [ -e "$${saved_rc}" ] || [ -L "$${saved_rc}" ]; then
		[ -L "$${saved_rc}" ] && \
			[ "$$(readlink "$${saved_rc}")" = ../init.d/meduza ] || exit 1
	else
		ln -s ../init.d/meduza "$${saved_rc}" || exit 1
	fi
	[ ! -e "$${rc_disabled}" ] && [ ! -L "$${rc_disabled}" ] || exit 1
	if [ -e "$${rc_link}" ] || [ -L "$${rc_link}" ]; then
		[ -L "$${rc_link}" ] || exit 1
		case "$$(readlink "$${rc_link}")" in ../init.d/meduza|/etc/init.d/meduza) :;; *) exit 1;; esac
		rm -f "$${rc_link}" || exit 1
		fsync /etc/rc.d || exit 1
	fi
	fsync "$${managed}" || exit 1
	meduza_atomic_value "ready:$${tx_nonce}:$${new_build}" "$${upgrade_state}" || exit 1
	exit 0
fi

# Disable boot activation before publishing any upgrade state.  Moving the
# exact rc.d symlink into the persistent journal is one atomic namespace
# operation: before it happens the old payload is untouched and safe to boot;
# after its directory fsync the old init cannot restart during an unpack/power
# loss window.  The new init restores the original enabled/disabled state only
# after its procd instance is registered.
[ -d /etc/rc.d ] && [ ! -L /etc/rc.d ] || exit 1
[ ! -L "$${rc_disabled}" ] || exit 1
if [ -e "$${saved_rc}" ] || [ -L "$${saved_rc}" ]; then
	[ -L "$${saved_rc}" ] || exit 1
	case "$$(readlink "$${saved_rc}")" in ../init.d/meduza|/etc/init.d/meduza) :;; *) exit 1;; esac
	if [ -e "$${rc_link}" ] || [ -L "$${rc_link}" ]; then
		[ -L "$${rc_link}" ] || exit 1
		case "$$(readlink "$${rc_link}")" in ../init.d/meduza|/etc/init.d/meduza) :;; *) exit 1;; esac
		rm -f "$${rc_link}" || exit 1
		fsync /etc/rc.d || exit 1
	fi
elif [ -e "$${rc_disabled}" ]; then
	[ -f "$${rc_disabled}" ] && [ "$$(cat "$${rc_disabled}")" = disabled ] || exit 1
	if [ -e "$${rc_link}" ] || [ -L "$${rc_link}" ]; then
		[ -L "$${rc_link}" ] || exit 1
		case "$$(readlink "$${rc_link}")" in ../init.d/meduza|/etc/init.d/meduza) :;; *) exit 1;; esac
		rm -f "$${rc_link}" || exit 1
		fsync /etc/rc.d || exit 1
	fi
elif [ -e "$${rc_link}" ] || [ -L "$${rc_link}" ]; then
	[ -L "$${rc_link}" ] || exit 1
	case "$$(readlink "$${rc_link}")" in ../init.d/meduza|/etc/init.d/meduza) :;; *) exit 1;; esac
	mv "$${rc_link}" "$${saved_rc}" || exit 1
	fsync /etc/rc.d || exit 1
	fsync "$${managed}" || exit 1
else
	meduza_atomic_value disabled "$${rc_disabled}" || exit 1
fi

# An immutable generation intent distinguishes the first capture from a retry
# after a power loss.  A retry that only finds `pending` (or the blocked state
# without any intent) must disable tagless edge migration; it may never rebuild
# authority from a later live firewall state.
[ ! -L "$${upgrade_intent}" ] || exit 1
intent_new=0
force_disable=0
if [ -e "$${upgrade_intent}" ]; then
	[ -f "$${upgrade_intent}" ] || exit 1
	intent_line=$$(cat "$${upgrade_intent}") || exit 1
	set -- $${intent_line}
	[ "$$#" -ge 3 ] && [ "$$1" = v1 ] || exit 1
	intent_nonce=$$2
	intent_phase=$$3
	case "$${intent_nonce}" in *[!0-9a-f]*|'') exit 1;; esac
	[ "$${#intent_nonce}" -eq 32 ] || exit 1
	case "$${intent_phase}" in
		bound)
			[ "$$#" -eq 4 ] || exit 1
			intent_input=$$4
			case "$${intent_input}" in *[!0-9a-f]*|'') exit 1;; esac
			[ "$${#intent_input}" -eq 64 ] || exit 1
			# A bound intent observed at process entry is necessarily a replay
			# after a crash.  A tagless token has no identity across an ABA, even
			# when every byte returned to its old value, so never migrate it.
			force_disable=1
			;;
		pending)
			[ "$$#" -eq 3 ] || exit 1
			force_disable=1
			;;
		disabled)
			[ "$$#" -eq 3 ] || exit 1
			force_disable=1
			;;
		*) exit 1;;
	esac
else
	intent_nonce=$$(python3 -c 'import secrets; print(secrets.token_hex(16))') || exit 1
	intent_tmp=$${upgrade_intent}.new.$$$$
	printf 'v1\t%s\tpending\n' "$${intent_nonce}" >"$${intent_tmp}" || exit 1
	chmod 600 "$${intent_tmp}" || exit 1
	fsync "$${intent_tmp}" || exit 1
	ln "$${intent_tmp}" "$${upgrade_intent}" 2>/dev/null || exit 1
	fsync "$${managed}" || exit 1
	rm -f "$${intent_tmp}" || exit 1
	fsync "$${managed}" || exit 1
	intent_phase=pending
	intent_new=1
	[ "$${had_state}" -eq 0 ] || force_disable=1
fi

[ ! -x /etc/init.d/meduza ] || /etc/init.d/meduza stop >/dev/null 2>&1 || true
if meduza_agent_registered; then
	ubus -S call service delete '{"name":"meduza","instance":"agent"}' >/dev/null 2>&1 || true
else
	status=$$?
	[ "$${status}" -eq 1 ] || exit 1
fi
meduza_signal_processes TERM

# Prove both the procd registration and every old controller/generator process
# are gone.  Two quiet observations avoid accepting a transient exit window.
quiet=0
attempt=0
while [ "$${attempt}" -lt 15 ]; do
	if meduza_agent_registered; then
		ubus -S call service delete '{"name":"meduza","instance":"agent"}' >/dev/null 2>&1 || true
		quiet=0
	else
		status=$$?
		[ "$${status}" -eq 1 ] || exit 1
		if meduza_process_live; then quiet=0; else quiet=$$((quiet + 1)); fi
	fi
	[ "$${quiet}" -ge 2 ] && break
	attempt=$$((attempt + 1))
	[ "$${attempt}" -ne 8 ] || meduza_signal_processes KILL
	sleep 1
done
[ "$${quiet}" -ge 2 ] || exit 1

# The previous owner-marker release did not persist firewall-edge generations.
# Seal the exact old (zone, logical) pairs only once.  A retry whose committed
# inputs differ disables tagless migration instead of claiming a later user or
# OpenClash membership.
if [ ! -e /etc/meduza/managed/uci-ownership.json ]; then
	[ -f /etc/config/meduza ] && [ ! -L /etc/config/meduza ] || exit 1
	for package in meduza firewall network openvpn; do
		[ -e "/etc/config/$${package}" ] || continue
		[ -f "/etc/config/$${package}" ] && [ ! -L "/etc/config/$${package}" ] || exit 1
		changes=$$(uci -q changes "$${package}" 2>/dev/null) || exit 1
		[ -z "$${changes}" ] || exit 1
	done
	if zone=$$(uci -q get meduza.main.VPN_FIREWALL_ZONE 2>/dev/null); then
		case "$${zone}" in *[!A-Za-z0-9_-]*) exit 1;; esac
	else
		uci -q show meduza.main >/dev/null 2>&1 || exit 1
		zone=
	fi
	if [ -e /etc/config/firewall ]; then have_firewall=1; else have_firewall=0; fi
	[ ! -L "$${migration}" ] && [ ! -L "$${seal}" ] && [ ! -L "$${disabled}" ] || exit 1

	raw=$${managed}/uci-migration.zone.raw.$$$$
	candidate=$${managed}/uci-migration.zone.candidate.$$$$
	: >"$${raw}" || exit 1
	input_before=$$(meduza_upgrade_input_hash) || exit 1
	if [ -n "$${zone}" ] && [ "$${have_firewall}" = 1 ]; then
		. /lib/functions.sh || exit 1
		matched=0
		members=
		meduza_capture_zone() {
			local section="$$1" name value
			config_get name "$${section}" name
			[ "$${name}" = "$${zone}" ] || return 0
			matched=$$((matched + 1))
			config_get value "$${section}" network
			members="$${value}"
		}
		config_load firewall || exit 1
		config_foreach meduza_capture_zone zone
		[ "$${matched}" -le 1 ] || exit 1
		tab=$$(printf '\t')
		for source in "$${managed}/interfaces" "$${managed}/interfaces.pending"; do
			[ ! -L "$${source}" ] || exit 1
			[ -f "$${source}" ] || continue
			while IFS="$${tab}" read -r kind instance logical device config extra; do
				[ -z "$${extra}" ] || continue
				case "$${kind}" in tinc|openvpn|wireguard) :;; *) continue;; esac
				case "$${logical}" in ''|*[!A-Za-z0-9_]*) continue;; esac
				case " $${members} " in
					*" $${logical} "*) printf '%s\t%s\n' "$${zone}" "$${logical}" >>"$${raw}" || exit 1;;
				esac
			done <"$${source}"
		done
	fi
	sort -u "$${raw}" >"$${candidate}" || exit 1
	rm -f "$${raw}" || exit 1
	input_after=$$(meduza_upgrade_input_hash) || exit 1
	[ "$${input_before}" = "$${input_after}" ] || exit 1
	candidate_hash=$$(meduza_file_hash "$${candidate}") || exit 1
	migration_disabled=$${force_disable}
	if [ "$${intent_phase}" = bound ]; then
		[ "$${intent_input}" = "$${input_before}" ] || migration_disabled=1
	elif [ "$${intent_phase}" = pending ] && [ "$${intent_new}" -eq 1 ]; then
		meduza_atomic_value "v1	$${intent_nonce}	bound	$${input_before}" "$${upgrade_intent}" || exit 1
		intent_phase=bound
		intent_input=$${input_before}
	else
		migration_disabled=1
	fi

	if [ -e "$${disabled}" ] || [ "$${migration_disabled}" -eq 1 ]; then
		if [ -e "$${disabled}" ]; then
			[ -f "$${disabled}" ] && [ ! -L "$${disabled}" ] || exit 1
			[ "$$(cat "$${disabled}")" = disabled ] || exit 1
		else
			meduza_atomic_value disabled "$${disabled}" || exit 1
		fi
		meduza_atomic_value "v1	$${intent_nonce}	disabled" "$${upgrade_intent}" || exit 1
		[ -f "$${disabled}" ] && [ ! -L "$${disabled}" ] || exit 1
		: >"$${candidate}" || exit 1
	elif [ -e "$${seal}" ]; then
		[ -f "$${seal}" ] && [ ! -L "$${seal}" ] || exit 1
		seal_line=$$(cat "$${seal}") || exit 1
		set -- $${seal_line}
		[ "$$#" -eq 4 ] && [ "$$1" = v1 ] || exit 1
		case "$$2" in *[!0-9a-f]*|'') exit 1;; esac
		[ "$${#2}" -eq 32 ] || exit 1
		case "$$3$$4" in *[!0-9a-f]*) exit 1;; esac
		[ "$${#3}" -eq 64 ] && [ "$${#4}" -eq 64 ] || exit 1
		if [ "$$2" != "$${intent_nonce}" ] || [ "$$3" != "$${input_before}" ] || [ "$$4" != "$${candidate_hash}" ]; then
			meduza_atomic_value disabled "$${disabled}" || exit 1
			meduza_atomic_value "v1	$${intent_nonce}	disabled" "$${upgrade_intent}" || exit 1
			: >"$${candidate}" || exit 1
		fi
	else
		seal_tmp=$${seal}.new.$$$$
		printf 'v1\t%s\t%s\t%s\n' "$${intent_nonce}" "$${input_before}" "$${candidate_hash}" >"$${seal_tmp}" || exit 1
		chmod 600 "$${seal_tmp}" || exit 1
		fsync "$${seal_tmp}" || exit 1
		ln "$${seal_tmp}" "$${seal}" 2>/dev/null || exit 1
		fsync "$${managed}" || exit 1
		rm -f "$${seal_tmp}" || exit 1
		fsync "$${managed}" || exit 1
	fi
	meduza_install_file "$${candidate}" "$${migration}" || exit 1
else
	# External per-section/edge generations already exist.  Never let a stale
	# one-shot snapshot become a second ownership authority on a later upgrade.
	meduza_atomic_value disabled "$${disabled}" || exit 1
	meduza_atomic_value "v1	$${intent_nonce}	disabled" "$${upgrade_intent}" || exit 1
fi

meduza_atomic_value "ready:$${tx_nonce}:$${new_build}" "$${upgrade_state}" || exit 1
exit 0
endef

define Build/Compile
endef

define Package/meduza-openwrt-lite/install
	$(INSTALL_DIR) $(1)/etc/config $(1)/etc/init.d $(1)/etc/meduza/managed $(1)/etc/meduza/generated $(1)/usr/libexec/meduza $(1)/usr/share/meduza $(1)/usr/share/luci/menu.d $(1)/usr/share/rpcd/acl.d $(1)/www/luci-static/resources/view/meduza
	$(INSTALL_CONF) ./files/etc/config/meduza $(1)/etc/config/meduza
	$(INSTALL_BIN) ./files/etc/init.d/meduza $(1)/etc/init.d/meduza
	$(INSTALL_BIN) ./files/usr/libexec/meduza/meduza-lib.sh $(1)/usr/libexec/meduza/meduza-lib.sh
	$(INSTALL_BIN) ./files/usr/libexec/meduza/meduza-agent.py $(1)/usr/libexec/meduza/meduza-agent.py
	$(INSTALL_BIN) ./files/usr/libexec/meduza/meduza-json.py $(1)/usr/libexec/meduza/meduza-json.py
	$(INSTALL_BIN) ./files/usr/libexec/meduza/meduza-recover $(1)/usr/libexec/meduza/meduza-recover
	$(INSTALL_BIN) ./files/usr/libexec/meduza/meduza-uci-session.py $(1)/usr/libexec/meduza/meduza-uci-session.py
	$(INSTALL_BIN) ./files/usr/libexec/meduza/meduza-generator $(1)/usr/libexec/meduza/meduza-generator
	$(INSTALL_BIN) ./files/usr/libexec/meduza/meduza-openwrt-sync $(1)/usr/libexec/meduza/meduza-openwrt-sync
	$(INSTALL_DATA) ./files/usr/share/meduza/openwrt-lite-build $(1)/usr/share/meduza/openwrt-lite-build
	$(INSTALL_DATA) ./files/usr/share/luci/menu.d/luci-app-meduza.json $(1)/usr/share/luci/menu.d/luci-app-meduza.json
	$(INSTALL_DATA) ./files/usr/share/rpcd/acl.d/luci-app-meduza.json $(1)/usr/share/rpcd/acl.d/luci-app-meduza.json
	$(INSTALL_DATA) ./files/www/luci-static/resources/view/meduza/settings.js $(1)/www/luci-static/resources/view/meduza/settings.js
endef

define Package/python3-etcd3/install
	$(INSTALL_DIR) $(1)/usr/lib/meduza-python
	$(CP) ./vendor/etcd3/. $(1)/usr/lib/meduza-python/
endef

define Package/python3-grpcio/install
	$(INSTALL_DIR) $(1)/usr/lib/meduza-python
	$(CP) ./vendor/grpcio/. $(1)/usr/lib/meduza-python/
endef


define Package/python3-protobuf/install
	$(INSTALL_DIR) $(1)/usr/lib/meduza-python
	$(CP) ./vendor/protobuf/. $(1)/usr/lib/meduza-python/
endef

define Package/python3-six/install
	$(INSTALL_DIR) $(1)/usr/lib/meduza-python
	$(CP) ./vendor/six/. $(1)/usr/lib/meduza-python/
endef

define Package/python3-typing-extensions/install
	$(INSTALL_DIR) $(1)/usr/lib/meduza-python
	$(CP) ./vendor/typing-extensions/. $(1)/usr/lib/meduza-python/
endef

define Package/python3-tenacity/install
	$(INSTALL_DIR) $(1)/usr/lib/meduza-python
	$(CP) ./vendor/tenacity/. $(1)/usr/lib/meduza-python/
endef

$(eval $(call BuildPackage,meduza-openwrt-lite))
$(eval $(call BuildPackage,python3-etcd3))
$(eval $(call BuildPackage,python3-grpcio))
$(eval $(call BuildPackage,python3-protobuf))
$(eval $(call BuildPackage,python3-six))
$(eval $(call BuildPackage,python3-typing-extensions))
$(eval $(call BuildPackage,python3-tenacity))
