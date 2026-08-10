include $(TOPDIR)/rules.mk

PKG_NAME:=meduza-openwrt-lite
PKG_VERSION:=0.1.0
PKG_RELEASE:=15

MEDUZA_PKGARCH?=$(ARCH_PACKAGES)

include $(INCLUDE_DIR)/package.mk

define Package/meduza-openwrt-lite
  SECTION:=net
  CATEGORY:=Network
  TITLE:=Lightweight Meduza etcd reconciler for OpenWrt
  PKGARCH:=all
  DEPENDS:=+python3 +python3-etcd3 +python3-grpcio +python3-protobuf \
    +python3-six +python3-typing-extensions
endef

define Package/python3-etcd3
  SECTION:=lang
  CATEGORY:=Languages
  SUBMENU:=Python
  TITLE:=Python etcd v3 client for Meduza
  PKGARCH:=all
  DEPENDS:=+python3 +python3-grpcio +python3-protobuf +python3-six
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

$(eval $(call BuildPackage,meduza-openwrt-lite))
$(eval $(call BuildPackage,python3-etcd3))
$(eval $(call BuildPackage,python3-grpcio))
$(eval $(call BuildPackage,python3-protobuf))
$(eval $(call BuildPackage,python3-six))
$(eval $(call BuildPackage,python3-typing-extensions))
