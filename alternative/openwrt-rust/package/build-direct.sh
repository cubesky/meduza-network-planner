#!/bin/sh
# Assemble a Meduza OpenWrt package from an already-built static executable.
# This intentionally does not run OpenWrt feeds or compile package dependencies.

set -eu

usage() {
	echo "usage: $0 <ipk|apk> <package-arch> <version> <binary> <sdk> <output>" >&2
	exit 2
}

[ "$#" -eq 6 ] || usage

format=$1
package_arch=$2
package_version=$3
binary=$4
sdk=$5
output=$6

package_name=meduza-openwrt-rust
description='Meduza OpenWrt VPN reconciler (Rust)'
dependencies='ca-bundle ip-full netifd rpcd ubus uci luci-base'

case "$format" in
	ipk|apk) ;;
	*) usage ;;
esac
case "$package_arch" in
	aarch64_cortex-a53|x86_64) ;;
	*) echo "unsupported package architecture: $package_arch" >&2; exit 1 ;;
esac
case "$package_version" in
	''|*[!A-Za-z0-9._+~-]*) echo 'invalid package version' >&2; exit 1 ;;
esac
[ -f "$binary" ] && [ ! -L "$binary" ] || {
	echo 'static executable is missing or is a symbolic link' >&2
	exit 1
}
[ -d "$sdk" ] || {
	echo 'OpenWrt SDK directory is missing' >&2
	exit 1
}
case "$output" in
	*.$format) ;;
	*) echo "output must end in .$format" >&2; exit 1 ;;
esac

work=$(mktemp -d "${TMPDIR:-/tmp}/meduza-package.XXXXXX")
trap 'rm -rf "$work"' EXIT HUP INT TERM
root=$work/root
scripts=$work/scripts
packages=$work/packages
mkdir -p "$root" "$scripts" "$packages" "$(dirname "$output")"

# Root filesystem payload. The executable is the only architecture-specific
# input; init, UCI, rpcd ACL and LuCI resources are data files.
install -d -m 0755 \
	"$root/usr/sbin" \
	"$root/etc/init.d" \
	"$root/etc/config" \
	"$root/usr/share/luci/menu.d" \
	"$root/usr/share/rpcd/acl.d" \
	"$root/www/luci-static/resources/view/meduza" \
	"$root/lib/upgrade/keep.d"
install -m 0755 "$binary" "$root/usr/sbin/meduza-openwrt"
install -m 0755 "$(dirname "$0")/files/etc/init.d/meduza" \
	"$root/etc/init.d/meduza"
install -m 0600 "$(dirname "$0")/files/etc/config/meduza" \
	"$root/etc/config/meduza"
install -m 0644 "$(dirname "$0")/files/usr/share/luci/menu.d/meduza.json" \
	"$root/usr/share/luci/menu.d/meduza.json"
install -m 0644 "$(dirname "$0")/files/usr/share/rpcd/acl.d/meduza.json" \
	"$root/usr/share/rpcd/acl.d/meduza.json"
install -m 0644 "$(dirname "$0")/files/www/luci-static/resources/view/meduza/settings.js" \
	"$root/www/luci-static/resources/view/meduza/settings.js"
printf '%s\n' /etc/meduza/ /etc/meduza-state/ \
	>"$root/lib/upgrade/keep.d/$package_name"

cat >"$scripts/preinst" <<'EOF'
#!/bin/sh
[ -n "${IPKG_INSTROOT:-}" ] && exit 0
[ "${PKG_UPGRADE:-0}" = 1 ] || exit 0
[ ! -x /etc/init.d/meduza ] || /etc/init.d/meduza stop >/dev/null 2>&1 || exit 1
exit 0
EOF

cat >"$scripts/prerm-pkg" <<'EOF'
#!/bin/sh
[ -n "${IPKG_INSTROOT:-}" ] && exit 0

meduza_controller_registered() {
	local services
	services="$(ubus -S call service list '{"name":"meduza"}' 2>/dev/null)" || return 2
	case "$services" in
		*'"meduza"'*) return 0 ;;
		*) return 1 ;;
	esac
}

# APK default_prerm normally stopped the service before this hook, whereas
# IPK normally invokes this hook first. Query procd so both orders are
# idempotent, and retry a stop only when the exact service remains registered.
if meduza_controller_registered; then
	/etc/init.d/meduza stop >/dev/null 2>&1 || exit 1
else
	status=$?
	[ "$status" -eq 1 ] || exit "$status"
fi
[ "${PKG_UPGRADE:-0}" = 1 ] && exit 0
[ -x /usr/sbin/meduza-openwrt ] || exit 1
echo 'meduza: removing managed VPN, FRR, firewall and runtime state' >&2
/usr/sbin/meduza-openwrt purge || exit 1
[ ! -x /etc/init.d/meduza ] || /etc/init.d/meduza disable >/dev/null 2>&1 || exit 1
exit 0
EOF
chmod 0755 "$scripts/preinst" "$scripts/prerm-pkg"

build_ipk() {
	fakeroot=$(command -v fakeroot || true)
	bash=$(command -v bash || true)
	ipkg_build=$sdk/scripts/ipkg-build
	[ -x "$fakeroot" ] && [ -x "$bash" ] && [ -f "$ipkg_build" ] || {
		echo 'IPK packaging tools are missing' >&2
		exit 1
	}

	control=$root/CONTROL
	mkdir -p "$control"
	cat >"$control/control" <<EOF
Package: $package_name
Version: $package_version
Depends: ca-bundle, ip-full, netifd, rpcd, ubus, uci, luci-base
Conflicts: meduza-openwrt-lite
Source: $package_name
SourceName: $package_name
License: MIT
LicenseFiles: LICENSE
Section: net
URL: https://github.com/cubesky/meduza-network-planner
Maintainer: Meduza
Architecture: $package_arch
Installed-Size: 0
Description: $description
 A statically linked controller that directly manages Meduza VPN interfaces.
EOF
	printf '%s\n' /etc/config/meduza >"$control/conffiles"
	cp "$scripts/preinst" "$control/preinst"
	cp "$scripts/prerm-pkg" "$control/prerm-pkg"
	cat >"$control/postinst" <<'EOF'
#!/bin/sh
[ "${IPKG_NO_SCRIPT:-}" = "1" ] && exit 0
[ -n "${IPKG_INSTROOT:-}" ] && exit 0
rm -f /tmp/luci-indexcache.*
if [ "${PKG_UPGRADE:-0}" != 1 ]; then
	/etc/init.d/meduza enable >/dev/null 2>&1 || exit 1
fi
# Starting an enabled procd service is a short registration call. Do not run
# disabled-mode purge/cleanup inside the package manager transaction.
if [ "$(uci -q get meduza.main.enable 2>/dev/null)" = 1 ]; then
	/etc/init.d/meduza start >/dev/null 2>&1 || \
		logger -t meduza "post-install start failed; start Meduza manually or reboot"
fi
exit 0
EOF
	cat >"$control/prerm" <<'EOF'
#!/bin/sh
[ -s "${IPKG_INSTROOT:-}/lib/functions.sh" ] || exit 0
. "${IPKG_INSTROOT:-}/lib/functions.sh"
default_prerm "$0" "$@"
EOF
	chmod 0755 "$control/preinst" "$control/postinst" \
		"$control/prerm" "$control/prerm-pkg"

	"$fakeroot" "$bash" "$ipkg_build" "$root" "$packages"
	set -- "$packages/${package_name}_${package_version}_${package_arch}.ipk"
	[ -f "$1" ] || {
		echo 'IPK packager did not produce the expected file' >&2
		exit 1
	}
	mv "$1" "$output"
}

build_apk() {
	fakeroot=$(command -v fakeroot || true)
	apk=$sdk/staging_dir/host/bin/apk
	[ -x "$fakeroot" ] && [ -x "$apk" ] || {
		echo 'APK packaging tools are missing' >&2
		exit 1
	}

	metadata=$root/lib/apk/packages
	mkdir -p "$metadata"
	# OpenWrt records the payload before adding its package-manager metadata.
	(
		cd "$root"
		find . \( -type f -o -type l \) -printf '/%P\n' | sort \
			>"lib/apk/packages/$package_name.list"
	)
	printf '%s\n' /etc/config/meduza /etc/meduza/ /etc/meduza-state/ \
		>"$metadata/$package_name.conffiles"
	config_hash=$(sha256sum "$root/etc/config/meduza" | awk '{print $1}')
	printf '/etc/config/meduza %s\n' "$config_hash" \
		>"$metadata/$package_name.conffiles_static"

	cp "$scripts/preinst" "$scripts/pre-install"
	{
		echo '#!/bin/sh'
		echo 'export PKG_UPGRADE=1'
		sed '1{/^#!/d;}' "$scripts/preinst"
	} >"$scripts/pre-upgrade"
	cat >"$scripts/post-install" <<'EOF'
#!/bin/sh
[ "${IPKG_NO_SCRIPT:-}" = "1" ] && exit 0
[ -n "${IPKG_INSTROOT:-}" ] && exit 0
rm -f /tmp/luci-indexcache.*
if [ "${PKG_UPGRADE:-0}" != 1 ]; then
	/etc/init.d/meduza enable >/dev/null 2>&1 || exit 1
fi
# Starting an enabled procd service is a short registration call. Do not run
# disabled-mode purge/cleanup inside the package manager transaction.
if [ "$(uci -q get meduza.main.enable 2>/dev/null)" = 1 ]; then
	/etc/init.d/meduza start >/dev/null 2>&1 || \
		logger -t meduza "post-install start failed; start Meduza manually or reboot"
fi
exit 0
EOF
	{
		echo '#!/bin/sh'
		echo 'export PKG_UPGRADE=1'
		sed '1{/^#!/d;}' "$scripts/post-install"
	} >"$scripts/post-upgrade"
	cat >"$scripts/pre-deinstall" <<'EOF'
#!/bin/sh
[ -s "${IPKG_INSTROOT:-}/lib/functions.sh" ] || exit 0
. "${IPKG_INSTROOT:-}/lib/functions.sh"
export root="${IPKG_INSTROOT:-}"
export pkgname="meduza-openwrt-rust"
echo 'meduza: stopping controller before package removal' >&2
# default_prerm emits an alarming ubus Not found diagnostic when procd has
# already removed the service. Its return value does not represent stop()
# failures; the following owner-aware purge is the authoritative cleanup and
# will fail closed if any managed runtime could not be removed.
default_prerm >/dev/null 2>&1 || true
EOF
	sed '1{/^#!/d;}' "$scripts/prerm-pkg" >>"$scripts/pre-deinstall"
	chmod 0755 "$scripts/pre-install" "$scripts/pre-upgrade" \
		"$scripts/post-install" "$scripts/post-upgrade" "$scripts/pre-deinstall"

	"$fakeroot" "$apk" mkpkg \
		--info "name:$package_name" \
		--info "version:$package_version" \
		--info "description:$description" \
		--info "arch:$package_arch" \
		--info 'license:MIT' \
		--info "origin:$package_name" \
		--info 'url:https://github.com/cubesky/meduza-network-planner' \
		--info 'maintainer:Meduza' \
		--script "pre-install:$scripts/pre-install" \
		--script "post-install:$scripts/post-install" \
		--script "pre-upgrade:$scripts/pre-upgrade" \
		--script "post-upgrade:$scripts/post-upgrade" \
		--script "pre-deinstall:$scripts/pre-deinstall" \
		--info "depends:$dependencies" \
		--files "$root" \
		--output "$output"
	[ -f "$output" ] || {
		echo 'APK packager did not produce the expected file' >&2
		exit 1
	}
}

case "$format" in
	ipk) build_ipk ;;
	apk) build_apk ;;
esac

sha256sum "$output" >"$output.sha256"
