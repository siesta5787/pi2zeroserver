################################################################################
#
# tailscale-static
#
################################################################################

# The same official tarballs the on-device updater downloads
TAILSCALE_STATIC_VERSION = 1.104.1
TAILSCALE_STATIC_SOURCE = tailscale_$(TAILSCALE_STATIC_VERSION)_arm64.tgz
TAILSCALE_STATIC_SITE = https://pkgs.tailscale.com/stable
TAILSCALE_STATIC_LICENSE = BSD-3-Clause

define TAILSCALE_STATIC_INSTALL_TARGET_CMDS
	$(INSTALL) -D -m 0755 $(@D)/tailscaled $(TARGET_DIR)/usr/lib/box/tailscale/tailscaled
	$(INSTALL) -D -m 0755 $(@D)/tailscale $(TARGET_DIR)/usr/lib/box/tailscale/tailscale
	echo $(TAILSCALE_STATIC_VERSION) > $(TARGET_DIR)/usr/lib/box/tailscale/VERSION
endef

$(eval $(generic-package))
