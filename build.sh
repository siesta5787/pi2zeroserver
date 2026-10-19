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

# Version stamped into /etc/box-version and the RAUC bundle. Sorts by build time.
dirty="$(git -C "${PROJECT_DIR}" diff --quiet HEAD -- 2>/dev/null || echo -dirty)"
export BOX_VERSION="${BOX_VERSION:-$(date -u +%Y.%m.%d.%H%M)-g$(git -C "${PROJECT_DIR}" rev-parse --short HEAD)${dirty}}"

# RAUC signing key. Dev builds get a self-signed key generated on first use;
# devices only accept bundles signed by the key their image was built with,
# so keep local/rauc/ backed up or dev devices will need reflashing.
if [ -z "${BOX_RAUC_KEY:-}" ]; then
	BOX_RAUC_KEY="${PROJECT_DIR}/local/rauc/${VARIANT}-key.pem"
	BOX_RAUC_CERT="${PROJECT_DIR}/local/rauc/${VARIANT}-cert.pem"
	if [ ! -f "${BOX_RAUC_KEY}" ]; then
		[ "${VARIANT}" = dev ] || {
			echo "No release signing key: set BOX_RAUC_KEY and BOX_RAUC_CERT" >&2; exit 1; }
		mkdir -p "${PROJECT_DIR}/local/rauc"
		openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes \
			-days 36500 -subj "/O=Box/CN=Box ${VARIANT} update signing" \
			-keyout "${BOX_RAUC_KEY}" -out "${BOX_RAUC_CERT}"
		chmod 0600 "${BOX_RAUC_KEY}"
	fi
fi
export BOX_RAUC_KEY BOX_RAUC_CERT="${BOX_RAUC_CERT:?set BOX_RAUC_CERT with BOX_RAUC_KEY}"

OUT="${BUILD_ROOT}/output/${VARIANT}"
MAKE=(make -C "${BUILD_ROOT}/buildroot" O="${OUT}" BR2_EXTERNAL="${BUILD_ROOT}/br2-external")

if [ ! -f "${OUT}/.config" ]; then
	"${MAKE[@]}" "${DEFCONFIG}"
fi

# Per-package directories let top-level parallel make build packages concurrently
exec "${MAKE[@]}" -j"$(nproc)" "$@"
