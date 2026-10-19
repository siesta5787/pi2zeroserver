#!/bin/bash
# Runs after the rootfs is populated, before it is packed into an image.
# Arguments: $1 = target dir, $2 = build variant ("dev" or "release")
set -euo pipefail

TARGET="$1"
VARIANT="${2:-release}"
SYSTEMCTL="${HOST_DIR}/bin/systemctl"

"${SYSTEMCTL}" --root="${TARGET}" enable \
	iwd.service systemd-networkd.service systemd-resolved.service \
	systemd-timesyncd.service data.mount \
	box-health-data.service box-mark-good.service

install -d -m 0755 "${TARGET}/data"

# RAUC only installs bundles signed by this certificate (set by build.sh)
install -D -m 0644 "${BOX_RAUC_CERT}" "${TARGET}/etc/rauc/keyring.pem"

echo "${BOX_VERSION}" > "${TARGET}/etc/box-version"

# No RTC on the Pi: systemd never sets the clock earlier than this file's
# mtime, so the clock starts at least at build time. Without this, a boot with
# no network sits in the past and update signatures look "not yet valid".
touch "${TARGET}/usr/lib/clock-epoch"

[ "${VARIANT}" = dev ] || exit 0

# ---- dev builds only: console login on HDMI, Wi-Fi from the gitignored local/ dir
LOCAL_DIR="${BOX_LOCAL_DIR:-$(dirname "$0")/../../../local}"

"${SYSTEMCTL}" --root="${TARGET}" enable getty@tty1.service

if [ -f "${LOCAL_DIR}/wifi.env" ]; then
	# shellcheck disable=SC1091
	. "${LOCAL_DIR}/wifi.env"
	# iwd names the file after the SSID, hex-encoded if it has unusual characters
	if [[ "${WIFI_SSID}" =~ ^[A-Za-z0-9_\ -]+$ ]]; then
		name="${WIFI_SSID}"
	else
		name="=$(printf '%s' "${WIFI_SSID}" | od -An -tx1 | tr -d ' \n')"
	fi
	install -d -m 0700 "${TARGET}/var/lib/iwd"
	printf '[Security]\nPassphrase=%s\n' "${WIFI_PASSWORD}" \
		> "${TARGET}/var/lib/iwd/${name}.psk"
	chmod 0600 "${TARGET}/var/lib/iwd/${name}.psk"
else
	echo "post-build: no ${LOCAL_DIR}/wifi.env - image will have no Wi-Fi configured" >&2
fi

