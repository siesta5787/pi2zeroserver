#!/bin/bash
# Runs after the rootfs is populated, before it is packed into an image.
# Arguments: $1 = target dir, $2 = build variant ("dev" or "release")
set -euo pipefail

TARGET="$1"
VARIANT="${2:-release}"
SYSTEMCTL="${HOST_DIR}/bin/systemctl"

"${SYSTEMCTL}" --root="${TARGET}" enable \
	iwd.service systemd-networkd.service systemd-resolved.service \
	systemd-timesyncd.service \
	tailscaled.service box-agent.service box-health-agent.service box-app.service \
	box-grow-data.service box-health-data.service box-health-tailscale.service \
	box-mark-good.service box-recovery.service box-firewall.service

install -d -m 0755 "${TARGET}/data"

# RAUC only installs bundles signed by this certificate (set by build.sh)
install -D -m 0644 "${BOX_RAUC_CERT}" "${TARGET}/etc/rauc/keyring.pem"

echo "${BOX_VERSION}" > "${TARGET}/etc/box-version"

# Where to look for updates, and the key their manifest must be signed with
install -d -m 0755 "${TARGET}/etc/box"
echo "${BOX_MANIFEST_URL}" > "${TARGET}/etc/box/manifest-url"
echo "${BOX_MANIFEST_TESTING_URL}" > "${TARGET}/etc/box/manifest-testing-url"
echo "${BOX_MANIFEST_PUB}" > "${TARGET}/etc/box/manifest-key.pub"

# No RTC on the Pi: systemd never sets the clock earlier than this file's
# mtime, so the clock starts at least at build time. Without this, a boot with
# no network sits in the past and update signatures look "not yet valid".
touch "${TARGET}/usr/lib/clock-epoch"

if [ "${VARIANT}" = dev ]; then
	# Dev builds: console login on HDMI
	"${SYSTEMCTL}" --root="${TARGET}" enable getty@tty1.service
else
	# Release builds: no login prompt anywhere, also not the one systemd's
	# getty generator would start on the kernel's serial console (root is locked
	# anyway; kernel and box messages still show on the screen)
	ln -sf /dev/null "${TARGET}/etc/systemd/system/serial-getty@.service"
	ln -sf /dev/null "${TARGET}/etc/systemd/system/getty@.service"
fi
