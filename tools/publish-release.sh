#!/bin/bash
# Publish a release: the OS bundle from a build, plus a signed manifest that
# also pins the app. Devices find it at
#   https://github.com/siesta5787/pi2zeroserver/releases/latest/download/manifest.json
#
# Usage: tools/publish-release.sh [--app-tag vX.Y.Z] [--allow-dirty] [--dry-run] [variant]
#   --app-tag      kid-phone-server release to pin (default: its latest release)
#   --allow-dirty  publish a build made from uncommitted changes
#   --dry-run      write and verify the manifest, but don't create the release
set -euo pipefail

PROJECT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BUILD_ROOT="${BOX_BUILD_ROOT:-/media/me/Claude/rpi-build}"
REPO="siesta5787/pi2zeroserver"
APP_REPO="siesta5787/kid-phone-server"
APP_ASSET="kid-phone-server-aarch64-unknown-linux-musl.tar.gz"

app_tag="" allow_dirty="" dry_run="" variant=dev
while [ $# -gt 0 ]; do
	case "$1" in
	--app-tag) app_tag="$2"; shift ;;
	--allow-dirty) allow_dirty=1 ;;
	--dry-run) dry_run=1 ;;
	-*) echo "unknown option $1" >&2; exit 2 ;;
	*) variant="$1" ;;
	esac
	shift
done

images="${BUILD_ROOT}/output/${variant}/images"
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

work="$(mktemp -d)"
trap 'rm -rf "${work}"' EXIT

[ -n "${app_tag}" ] || app_tag="$(gh release view -R "${APP_REPO}" --json tagName -q .tagName)"
app_url="https://github.com/${APP_REPO}/releases/download/${app_tag}/${APP_ASSET}"
echo "Pinning ${APP_REPO} ${app_tag}"
curl -fsSL -o "${work}/app.tgz" "${app_url}"
app_sha256="$(sha256sum "${work}/app.tgz" | cut -d' ' -f1)"

bundle_url="https://github.com/${REPO}/releases/download/${tag}/$(basename "${bundle}")"
python3 -I - "${work}/manifest.json" "${os_version}" "${bundle_url}" "${app_tag#v}" "${app_url}" "${app_sha256}" <<'PY'
import json, sys
path, os_version, bundle_url, app_version, app_url, app_sha256 = sys.argv[1:]
manifest = {
    "schema": 1,
    "os": {"version": os_version, "bundle_url": bundle_url},
    "app": {"name": "kid-phone-server", "version": app_version, "url": app_url, "sha256": app_sha256},
}
with open(path, "w") as f:
    json.dump(manifest, f, indent=2)
    f.write("\n")
PY
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

gh release create "${tag}" -R "${REPO}" --title "Box ${os_version}" \
	--notes "OS ${os_version}; app kid-phone-server ${app_tag}" \
	"${bundle}" "${work}/manifest.json" "${work}/manifest.json.sig"
echo "Published ${tag}"
