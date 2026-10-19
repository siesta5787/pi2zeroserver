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

# genimage copies --rootpath into its tmp dir; give it an empty one. The rootfs
# is already built, and the data partition should start empty.
ROOTPATH_TMP="$(mktemp -d)"
BUNDLE_TMP="$(mktemp -d)"
trap 'rm -rf "${ROOTPATH_TMP}" "${BUNDLE_TMP}"' EXIT

rm -rf "${GENIMAGE_TMP}"
genimage \
	--rootpath "${ROOTPATH_TMP}" \
	--tmppath "${GENIMAGE_TMP}" \
	--inputpath "${BINARIES_DIR}" \
	--outputpath "${BINARIES_DIR}" \
	--config "${BOARD_DIR}/genimage.cfg"

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
