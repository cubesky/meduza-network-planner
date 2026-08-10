include $(TOPDIR)/rules.mk

PKG_NAME:=meduza-openwrt-lite
PKG_VERSION:=0.1.0
PKG_RELEASE:=10

include $(INCLUDE_DIR)/package.mk

define Package/meduza-openwrt-lite
  SECTION:=net
  CATEGORY:=Network
  TITLE:=Lightweight Meduza etcd reconciler for OpenWrt
  EXTRA_DEPENDS:=ca-bundle (>=0), python3 (>=0), jq (>=0), libstdcpp6 (>=0), \
    procps-ng-pgrep (>=0), frr (>=0), openvpn-openssl (>=0), \
    tinc (>=0), wireguard-tools (>=0), kmod-wireguard (>=0), luci-base (>=0)
endef

define Package/meduza-openwrt-lite/description
 Pulls Meduza's declarative configuration from etcd and configures the native
 OpenWrt tinc, FRR, OpenVPN and WireGuard packages. Reports node and tunnel
 health back to etcd through a resilient Python agent without a container runtime.
endef

define Package/meduza-openwrt-lite/conffiles
/etc/config/meduza
endef

define Build/Compile
endef

define Package/meduza-openwrt-lite/install
	$(INSTALL_DIR) $(1)/etc/config $(1)/etc/init.d $(1)/usr/libexec/meduza $(1)/usr/share/luci/menu.d $(1)/usr/share/rpcd/acl.d $(1)/www/luci-static/resources/view/meduza
	$(INSTALL_DIR) $(1)/usr/lib/meduza-python
	$(CP) ./vendor/. $(1)/usr/lib/meduza-python/
	$(INSTALL_CONF) ./files/etc/config/meduza $(1)/etc/config/meduza
	$(INSTALL_BIN) ./files/etc/init.d/meduza $(1)/etc/init.d/meduza
	$(INSTALL_BIN) ./files/usr/libexec/meduza/meduza-lib.sh $(1)/usr/libexec/meduza/meduza-lib.sh
	$(INSTALL_BIN) ./files/usr/libexec/meduza/meduza-agent.py $(1)/usr/libexec/meduza/meduza-agent.py
	$(INSTALL_BIN) ./files/usr/libexec/meduza/meduza-generator $(1)/usr/libexec/meduza/meduza-generator
	$(INSTALL_BIN) ./files/usr/libexec/meduza/meduza-openwrt-sync $(1)/usr/libexec/meduza/meduza-openwrt-sync
	$(INSTALL_DATA) ./files/usr/share/luci/menu.d/luci-app-meduza.json $(1)/usr/share/luci/menu.d/luci-app-meduza.json
	$(INSTALL_DATA) ./files/usr/share/rpcd/acl.d/luci-app-meduza.json $(1)/usr/share/rpcd/acl.d/luci-app-meduza.json
	$(INSTALL_DATA) ./files/www/luci-static/resources/view/meduza/settings.js $(1)/www/luci-static/resources/view/meduza/settings.js
endef

$(eval $(call BuildPackage,meduza-openwrt-lite))
