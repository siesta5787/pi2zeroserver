#!/bin/bash
# Assemble sdcard.img from the boot files, rootfs and an empty data partition.
set -euo pipefail

BOARD_DIR="$(dirname "$0")"
GENIMAGE_TMP="${BUILD_DIR}/genimage.tmp"

# genimage copies --rootpath into its tmp dir; give it an empty one. The rootfs
# is already built, and the data partition should start empty.
ROOTPATH_TMP="$(mktemp -d)"
trap 'rm -rf "${ROOTPATH_TMP}"' EXIT

rm -rf "${GENIMAGE_TMP}"
genimage \
	--rootpath "${ROOTPATH_TMP}" \
	--tmppath "${GENIMAGE_TMP}" \
	--inputpath "${BINARIES_DIR}" \
	--outputpath "${BINARIES_DIR}" \
	--config "${BOARD_DIR}/genimage.cfg"

# Compressed copy for Raspberry Pi Imager / balenaEtcher
xz -T0 -6 -f -k "${BINARIES_DIR}/sdcard.img"
