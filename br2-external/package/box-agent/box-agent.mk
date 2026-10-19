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

# Buildroot vendors crates for downloaded cargo packages only; local sources
# have to do it themselves. Crates are cached in the download dir, so this
# only touches the network when Cargo.lock changes.
define BOX_AGENT_VENDOR_CRATES
	cd $(@D) && rm -rf vendor .cargo && mkdir .cargo && \
		$(HOST_MAKE_ENV) CARGO_HOME=$(BR_CARGO_HOME) \
		cargo vendor --locked vendor > .cargo/config.toml
endef
BOX_AGENT_POST_RSYNC_HOOKS += BOX_AGENT_VENDOR_CRATES

$(eval $(cargo-package))
