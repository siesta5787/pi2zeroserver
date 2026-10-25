#!/bin/bash
# Publish a system release: the OS bundle from a build, plus a signed manifest.
# Release builds also attach the card image, box-<version>.img.xz, for new
# boxes (docs/setup-guide.md links releases/latest). Devices find it at
#   https://github.com/siesta5787/pi2zeroserver/releases/latest/download/manifest.json
# The app isn't part of it: each box runs the app it pinned, updated from that
# app's own releases (tools/app-release.sh, docs/box-ready-apps.md).
#
# Usage: tools/publish-release.sh [--prerelease] [--allow-dirty] [--dry-run] [variant]
#   --prerelease   a test version: only boxes whose owner opted in ("Get test
#                  versions early") install it. Published as a GitHub
#                  pre-release (never "latest"), and the rolling `testing`
#                  pre-release gets its manifest, which those boxes also read.
#   --allow-dirty  publish a build made from uncommitted changes
#   --dry-run      write and verify the manifest, but don't create the release
# variant: dev (default) or release; release reads its key from local/release.env
set -euo pipefail

PROJECT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BUILD_ROOT="${BOX_BUILD_ROOT:-/media/me/Claude/rpi-build}"
REPO="siesta5787/pi2zeroserver"

allow_dirty="" dry_run="" prerelease="" variant=dev
while [ $# -gt 0 ]; do
	case "$1" in
	--allow-dirty) allow_dirty=1 ;;
	--prerelease) prerelease=1 ;;
	--dry-run) dry_run=1 ;;
	-*) echo "unknown option $1" >&2; exit 2 ;;
	*) variant="$1" ;;
	esac
	shift
done

images="${BUILD_ROOT}/output/${variant}/images"
if [ "${variant}" = release ] && [ -z "${BOX_MANIFEST_KEY:-}" ] && [ -f "${PROJECT_DIR}/local/release.env" ]; then
	# shellcheck source=/dev/null
	. "${PROJECT_DIR}/local/release.env"
fi
key="${BOX_MANIFEST_KEY:-${PROJECT_DIR}/local/manifest/${variant}-key.pem}"
[ -f "${key}" ] || { echo "No manifest key at ${key}" >&2; exit 1; }

bundles=("${images}"/box-*.raucb)
[ "${#bundles[@]}" -eq 1 ] && [ -f "${bundles[0]}" ] || {
	echo "Expected exactly one bundle in ${images}" >&2; exit 1; }
bundle="${bundles[0]}"
os_version="$(basename "${bundle}" .raucb)"
os_version="${os_version#box-}"
case "${os_version}" in *-dirty)
	[ -n "${allow_dirty}" ] || {
		echo "Build ${os_version} has uncommitted changes; commit and rebuild, or pass --allow-dirty" >&2
		exit 1; } ;;
esac
tag="v${os_version}"
# The card image to flash, for new boxes. Release builds only: a dev image
# carries the Wi-Fi password from local/wifi.env on its data partition.
image="" image_name="box-${os_version}.img.xz"
if [ "${variant}" = release ]; then
	image="${images}/sdcard.img.xz"
	[ -f "${image}" ] || { echo "No card image at ${image}" >&2; exit 1; }
	# post-image.sh writes it just before the bundle; an older one is from another build
	[ $(($(stat -c %Y "${bundle}") - $(stat -c %Y "${image}"))) -le 600 ] || {
		echo "${image} is older than the bundle: rebuild" >&2; exit 1; }
fi
if gh release view "${tag}" -R "${REPO}" >/dev/null 2>&1; then
	echo "Version ${os_version} is already published: bump VERSION, commit, rebuild" >&2
	exit 1
fi

work="$(mktemp -d)"
trap 'rm -rf "${work}"' EXIT

bundle_url="https://github.com/${REPO}/releases/download/${tag}/$(basename "${bundle}")"
python3 -I - "${work}/manifest.json" "${os_version}" "${bundle_url}" <<'PY'
import json, sys
path, os_version, bundle_url = sys.argv[1:]
manifest = {"schema": 1, "os": {"version": os_version, "bundle_url": bundle_url}}
with open(path, "w") as f:
    json.dump(manifest, f, indent=2)
    f.write("\n")
PY
assets=("${bundle}" "${work}/manifest.json" "${work}/manifest.json.sig")
if [ -n "${image}" ] && [ -z "${dry_run}" ]; then
	cp "${image}" "${work}/${image_name}"
	assets+=("${work}/${image_name}")
fi
openssl pkeyutl -sign -inkey "${key}" -rawin -in "${work}/manifest.json" -out "${work}/manifest.json.sig"
openssl pkey -in "${key}" -pubout -out "${work}/pub.pem"
openssl pkeyutl -verify -pubin -inkey "${work}/pub.pem" -rawin \
	-in "${work}/manifest.json" -sigfile "${work}/manifest.json.sig" >/dev/null
cat "${work}/manifest.json"

if [ -n "${dry_run}" ]; then
	cp "${work}/manifest.json" "${work}/manifest.json.sig" "${images}/"
	echo "Dry run: manifest written to ${images}/ (not published)"
	exit 0
fi

if [ -n "${prerelease}" ]; then
	gh release create "${tag}" -R "${REPO}" --prerelease --title "Box ${os_version} (test version)" \
		--notes "Test version of the system: only boxes set to get test versions early install it. OS ${os_version}" \
		"${assets[@]}"
	# Opted-in boxes read releases/download/testing/manifest.json: point it here
	if gh release view testing -R "${REPO}" >/dev/null 2>&1; then
		gh release upload testing -R "${REPO}" --clobber "${work}/manifest.json" "${work}/manifest.json.sig"
	else
		gh release create testing -R "${REPO}" --prerelease --title "Newest test version (opt-in)" \
			--notes "Boxes set to get test versions early read this manifest. It points at the newest pre-release." \
			"${work}/manifest.json" "${work}/manifest.json.sig"
	fi
	echo "Published test version ${tag} (and pointed the testing manifest at it)"
else
	gh release create "${tag}" -R "${REPO}" --title "Box ${os_version}" \
		--notes "System ${os_version}. New box: flash ${image_name} (docs/setup-guide.md). Existing boxes update themselves." \
		"${assets[@]}"
	echo "Published ${tag}"
fi
