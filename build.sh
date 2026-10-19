#!/bin/bash
# Build wrapper. Buildroot can't build in a path with spaces, so the Buildroot
# tree, outputs and caches live in a sibling folder on the same drive:
#
#   /media/me/Claude/rpi-build/
#     buildroot/          pinned Buildroot LTS (fetched by this script)
#     br2-external ->     symlink to this project's br2-external/
#     output/<variant>/   one build tree per variant (dev, release)
#     cache/dl/           downloaded sources, shared by all variants
#     cache/ccache/       compiler cache, shared by all variants
#
# Usage: ./build.sh [variant] [buildroot make targets...]
#   ./build.sh                    build the dev image
#   ./build.sh dev menuconfig     change config (then: ./build.sh dev savedefconfig)
#   ./build.sh dev linux-rebuild  any Buildroot target works
set -euo pipefail

BUILDROOT_VERSION=2026.02.3
PROJECT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BUILD_ROOT="${BOX_BUILD_ROOT:-/media/me/Claude/rpi-build}"

VARIANT="${1:-dev}"
shift || true
DEFCONFIG="box_${VARIANT}_defconfig"
[ -f "${PROJECT_DIR}/br2-external/configs/${DEFCONFIG}" ] || {
	echo "Unknown variant '${VARIANT}' (no br2-external/configs/${DEFCONFIG})" >&2
	exit 1
}

case "${BUILD_ROOT}" in *" "*)
	echo "BOX_BUILD_ROOT must not contain spaces: ${BUILD_ROOT}" >&2; exit 1 ;;
esac

mkdir -p "${BUILD_ROOT}/cache/dl" "${BUILD_ROOT}/cache/ccache" "${BUILD_ROOT}/output"

if [ ! -d "${BUILD_ROOT}/buildroot/.git" ]; then
	git clone --depth 1 --branch "${BUILDROOT_VERSION}" \
		https://gitlab.com/buildroot.org/buildroot.git "${BUILD_ROOT}/buildroot"
fi
have="$(git -C "${BUILD_ROOT}/buildroot" describe --tags --exact-match 2>/dev/null || echo unknown)"
if [ "${have}" != "${BUILDROOT_VERSION}" ]; then
	echo "Buildroot in ${BUILD_ROOT}/buildroot is ${have}, expected ${BUILDROOT_VERSION}." >&2
	echo "Delete that folder and rerun to fetch the right version." >&2
	exit 1
fi

ln -sfn "${PROJECT_DIR}/br2-external" "${BUILD_ROOT}/br2-external"

export BR2_DL_DIR="${BUILD_ROOT}/cache/dl"
export BR2_CCACHE_DIR="${BUILD_ROOT}/cache/ccache"
export BOX_LOCAL_DIR="${PROJECT_DIR}/local"

OUT="${BUILD_ROOT}/output/${VARIANT}"
MAKE=(make -C "${BUILD_ROOT}/buildroot" O="${OUT}" BR2_EXTERNAL="${BUILD_ROOT}/br2-external")

if [ ! -f "${OUT}/.config" ]; then
	"${MAKE[@]}" "${DEFCONFIG}"
fi

# Per-package directories let top-level parallel make build packages concurrently
exec "${MAKE[@]}" -j"$(nproc)" "$@"
