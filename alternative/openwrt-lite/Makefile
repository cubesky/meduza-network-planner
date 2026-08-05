include $(TOPDIR)/rules.mk

PKG_NAME:=meduza-openwrt-lite
PKG_VERSION:=0.1.0
PKG_RELEASE:=1
PKGARCH:=all

include $(INCLUDE_DIR)/package.mk

define Package/meduza-openwrt-lite
  SECTION:=net
  CATEGORY:=Network
  TITLE:=Lightweight Meduza etcd reconciler for OpenWrt
  DEPENDS:=+ca-bundle +curl +jq +procps-ng-pgrep +frr +frr-vtysh +openvpn-openssl +tinc +wireguard-tools +kmod-wireguard +luci-base
endef

define Package/meduza-openwrt-lite/description
 Pulls Meduza's declarative configuration from etcd and configures the native
 OpenWrt tinc, FRR, OpenVPN and WireGuard packages. Reports node and tunnel
 health back to etcd without Python or a container runtime.
endef

define Package/meduza-openwrt-lite/conffiles
/etc/config/meduza
endef

define Build/Compile
endef

define Package/meduza-openwrt-lite/install
	$(INSTALL_DIR) $(1)/etc/config $(1)/etc/init.d $(1)/usr/libexec/meduza $(1)/usr/share/luci/menu.d $(1)/usr/share/rpcd/acl.d $(1)/www/luci-static/resources/view/meduza
	$(INSTALL_CONF) ./files/etc/config/meduza $(1)/etc/config/meduza
	$(INSTALL_BIN) ./files/etc/init.d/meduza $(1)/etc/init.d/meduza
	$(INSTALL_BIN) ./files/usr/libexec/meduza/meduza-lib.sh $(1)/usr/libexec/meduza/meduza-lib.sh
	$(INSTALL_BIN) ./files/usr/libexec/meduza/meduza-generator $(1)/usr/libexec/meduza/meduza-generator
	$(INSTALL_BIN) ./files/usr/libexec/meduza/meduza-reporter $(1)/usr/libexec/meduza/meduza-reporter
	$(INSTALL_BIN) ./files/usr/libexec/meduza/meduza-openwrt-sync $(1)/usr/libexec/meduza/meduza-openwrt-sync
	$(INSTALL_DATA) ./files/usr/share/luci/menu.d/luci-app-meduza.json $(1)/usr/share/luci/menu.d/luci-app-meduza.json
	$(INSTALL_DATA) ./files/usr/share/rpcd/acl.d/luci-app-meduza.json $(1)/usr/share/rpcd/acl.d/luci-app-meduza.json
	$(INSTALL_DATA) ./files/www/luci-static/resources/view/meduza/settings.js $(1)/www/luci-static/resources/view/meduza/settings.js
endef

$(eval $(call BuildPackage,meduza-openwrt-lite))
