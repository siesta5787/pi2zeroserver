#!/bin/bash
# Assemble sdcard.img (for flashing) and box-<version>.raucb (signed update).
set -euo pipefail

BOARD_DIR="$(dirname "$0")"
GENIMAGE_TMP="${BUILD_DIR}/genimage.tmp"

# U-Boot silently drops fragment options it doesn't know (they get renamed
# between releases). The environment settings must match /etc/fw_env.config or
# Linux and U-Boot can't read each other's A/B state, so check they all stuck.
UBOOT_CONFIG="$(ls -d "${BUILD_DIR}"/uboot-*/.config | grep -v tools)"
grep '^CONFIG_' "${BOARD_DIR}/uboot.fragment" | while read -r opt; do
	grep -qxF "${opt}" "${UBOOT_CONFIG}" || {
		echo "U-Boot config is missing '${opt}' from uboot.fragment (renamed option?)" >&2
		exit 1
	}
done

# genimage only uses --rootpath for the data partition (its "/data" subdir);
# the rootfs is already built. /data starts empty, except that dev builds get
# the Wi-Fi network from the gitignored local/wifi.env.
ROOTPATH_TMP="$(mktemp -d)"
BUNDLE_TMP="$(mktemp -d)"
trap 'rm -rf "${ROOTPATH_TMP}" "${BUNDLE_TMP}"' EXIT
install -d -m 0755 "${ROOTPATH_TMP}/data"

if [ "${2:-}" = dev ] && [ -f "${BOX_PROJECT_DIR}/local/wifi.env" ]; then
	# shellcheck disable=SC1091
	(. "${BOX_PROJECT_DIR}/local/wifi.env" && export WIFI_SSID WIFI_PASSWORD &&
		SET_WIFI_NO_ROOT=1 "${BOX_PROJECT_DIR}/tools/set-wifi.sh" "${ROOTPATH_TMP}/data")
fi

# fakeroot so files on the data partition are owned by root, not the build user
rm -rf "${GENIMAGE_TMP}"
fakeroot -- sh -c 'chown -R 0:0 "$1" && exec genimage \
	--rootpath "$1" \
	--tmppath "$2" \
	--inputpath "$3" \
	--outputpath "$3" \
	--config "$4"' \
	sh "${ROOTPATH_TMP}" "${GENIMAGE_TMP}" "${BINARIES_DIR}" "${BOARD_DIR}/genimage.cfg"

# Compressed copy for Raspberry Pi Imager / balenaEtcher
xz -T0 -6 -f -k "${BINARIES_DIR}/sdcard.img"

# RAUC update bundle containing the new rootfs (kernel included in /boot)
cp "${BINARIES_DIR}/rootfs.ext4" "${BUNDLE_TMP}/rootfs.ext4"
cat > "${BUNDLE_TMP}/manifest.raucm" <<MANIFEST
[update]
compatible=box-zero2w
version=${BOX_VERSION}

[bundle]
format=verity

[image.rootfs]
filename=rootfs.ext4
MANIFEST

rm -f "${BINARIES_DIR}"/box-*.raucb
"${HOST_DIR}/bin/rauc" bundle \
	--cert="${BOX_RAUC_CERT}" \
	--key="${BOX_RAUC_KEY}" \
	"${BUNDLE_TMP}" "${BINARIES_DIR}/box-${BOX_VERSION}.raucb"
