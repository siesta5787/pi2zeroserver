################################################################################
#
# box-agent
#
################################################################################

BOX_AGENT_VERSION = 0.1.0
BOX_AGENT_SITE = $(BR2_EXTERNAL_BOX_PATH)/../agent
BOX_AGENT_SITE_METHOD = local
BOX_AGENT_LICENSE = MIT
BOX_AGENT_OVERRIDE_SRCDIR_RSYNC_EXCLUSIONS = --exclude target --exclude vendor
# HTTPS downloads go through the system libcurl (OpenSSL, CA certificates)
BOX_AGENT_DEPENDENCIES = libcurl
# Let curl-sys find the target's libcurl through Buildroot's pkg-config
BOX_AGENT_CARGO_ENV = PKG_CONFIG_ALLOW_CROSS=1

# Buildroot vendors crates for downloaded cargo packages only; local sources
# have to do it themselves. Crates are cached in the download dir, so this
# only touches the network when Cargo.lock changes.
define BOX_AGENT_VENDOR_CRATES
	cd $(@D) && rm -rf vendor .cargo && mkdir .cargo && \
		$(HOST_MAKE_ENV) CARGO_HOME=$(BR_CARGO_HOME) \
		cargo vendor --locked vendor > .cargo/config.toml
endef
BOX_AGENT_POST_RSYNC_HOOKS += BOX_AGENT_VENDOR_CRATES

# curl-sys quietly builds its own curl, without HTTPS, if it can't find the
# system one; that would break every download. Refuse to install such a build.
define BOX_AGENT_CHECK_SYSTEM_LIBCURL
	$(TARGET_READELF) -d $(TARGET_DIR)/usr/bin/box-agent | grep -q 'NEEDED.*libcurl\.so' || \
		{ echo "box-agent doesn't link the system libcurl" >&2; exit 1; }
endef
BOX_AGENT_POST_INSTALL_TARGET_HOOKS += BOX_AGENT_CHECK_SYSTEM_LIBCURL

$(eval $(cargo-package))
