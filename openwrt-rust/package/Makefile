include $(TOPDIR)/rules.mk

PKG_NAME:=meduza-openwrt-rust
PKG_VERSION:=0.1.0
MEDUZA_PKG_RELEASE?=1
PKG_RELEASE:=$(MEDUZA_PKG_RELEASE)
PKG_LICENSE:=MIT
PKG_LICENSE_FILES:=LICENSE
PKG_MAINTAINER:=Meduza

# The Rust executable is built in a separate, pinned cross-compilation job and
# copied into prebuilt/ before this OpenWrt package is compiled.  Keeping Cargo
# outside the SDK avoids building a complete Rust host toolchain four times.
MEDUZA_RUST_BINARY?=$(CURDIR)/prebuilt/meduza-openwrt

include $(INCLUDE_DIR)/package.mk

define Package/meduza-openwrt-rust
  SECTION:=net
  CATEGORY:=Network
  TITLE:=Meduza OpenWrt VPN reconciler (Rust)
  URL:=https://github.com/cubesky/meduza-network-planner
  PKGARCH:=$(ARCH_PACKAGES)
  DEPENDS:=+ca-bundle +ip-full +rpcd +ubus +uci +luci-base
  CONFLICTS:=meduza-openwrt-lite
endef

define Package/meduza-openwrt-rust/description
 A statically linked Rust controller which directly owns Meduza's tinc,
 OpenVPN, WireGuard interfaces and FRR configuration without VPN UCI sections.
endef

define Package/meduza-openwrt-rust/conffiles
/etc/config/meduza
/etc/meduza/
/etc/meduza-state/
endef

define Package/meduza-openwrt-rust/preinst
#!/bin/sh
[ -n "$${IPKG_INSTROOT:-}" ] && exit 0
[ "$${PKG_UPGRADE:-0}" = 1 ] || exit 0
[ ! -x /etc/init.d/meduza ] || /etc/init.d/meduza stop >/dev/null 2>&1 || exit 1
exit 0
endef

define Package/meduza-openwrt-rust/prerm
#!/bin/sh
[ -n "$${IPKG_INSTROOT:-}" ] && exit 0
if [ -x /etc/init.d/meduza ]; then
	/etc/init.d/meduza stop >/dev/null 2>&1 || exit 1
fi
[ "$${PKG_UPGRADE:-0}" = 1 ] && exit 0
[ -x /usr/sbin/meduza-openwrt ] || exit 1
/usr/sbin/meduza-openwrt purge >/dev/null 2>&1 || exit 1
[ ! -x /etc/init.d/meduza ] || /etc/init.d/meduza disable >/dev/null 2>&1 || exit 1
exit 0
endef

define Build/Compile
	[ -f "$(MEDUZA_RUST_BINARY)" ]
	! $(TARGET_CROSS)readelf -l "$(MEDUZA_RUST_BINARY)" | grep -q 'Requesting program interpreter'
	! $(TARGET_CROSS)readelf -d "$(MEDUZA_RUST_BINARY)" 2>/dev/null | grep -q '(NEEDED)'
endef

define Package/meduza-openwrt-rust/install
	$(INSTALL_DIR) $(1)/usr/sbin $(1)/etc/init.d $(1)/etc/config
	$(INSTALL_DIR) $(1)/usr/share/luci/menu.d $(1)/usr/share/rpcd/acl.d
	$(INSTALL_DIR) $(1)/www/luci-static/resources/view/meduza
	$(INSTALL_BIN) $(MEDUZA_RUST_BINARY) $(1)/usr/sbin/meduza-openwrt
	$(INSTALL_BIN) ./files/etc/init.d/meduza $(1)/etc/init.d/meduza
	$(INSTALL_CONF) ./files/etc/config/meduza $(1)/etc/config/meduza
	$(INSTALL_DATA) ./files/usr/share/luci/menu.d/meduza.json $(1)/usr/share/luci/menu.d/meduza.json
	$(INSTALL_DATA) ./files/usr/share/rpcd/acl.d/meduza.json $(1)/usr/share/rpcd/acl.d/meduza.json
	$(INSTALL_DATA) ./files/www/luci-static/resources/view/meduza/settings.js $(1)/www/luci-static/resources/view/meduza/settings.js
endef

$(eval $(call BuildPackage,meduza-openwrt-rust))
