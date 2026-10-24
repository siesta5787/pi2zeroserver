#!/bin/bash
# Makes an app release box-ready: a signed box-app.json attached to the app's
# GitHub release, which boxes running that app install (see docs/box-ready-apps.md).
#
#   tools/app-release.sh new-key FILE           make an app signing key (keep it secret, back it up)
#   tools/app-release.sh fingerprint FILE       the key's fingerprint, for the app's README
#   tools/app-release.sh sign --key FILE --spec FILE --repo OWNER/REPO --tag vX.Y.Z
#                             [--tarball FILE] [--prerelease] [--dry-run] [--out DIR]
#
# sign: builds box-app.json from the spec (name, asset, command, env, secrets,
# first_login), the release's version (the tag without "v"), the asset's
# download address and SHA-256, and the key's public half; signs it; and
# uploads box-app.json + box-app.json.sig to that release.
#   --tarball     use this file instead of downloading the asset (must be identical)
#   --prerelease  a test version: also point the repo's rolling `box-testing`
#                 pre-release at it, which boxes set to get test versions read
#   --dry-run     don't upload; with --out, leave the files there
# Needs: openssl, python3, curl, gh (for uploading).
set -euo pipefail

die() { echo "$*" >&2; exit 1; }

raw_pub() { openssl pkey -in "$1" -pubout -outform DER | tail -c 32; }
fingerprint() { raw_pub "$1" | sha256sum | cut -c1-20 | sed 's/..../& /g; s/ $//'; }

case "${1:-}" in
new-key)
	[ -n "${2:-}" ] || die "usage: $0 new-key FILE"
	[ ! -e "$2" ] || die "$2 already exists; not overwriting it"
	(umask 077 && openssl genpkey -algorithm ed25519 -out "$2")
	echo "App signing key written to $2. Keep it secret and back it up: boxes running"
	echo "this app only accept releases signed with it."
	echo "Fingerprint (put it in the app's README): $(fingerprint "$2")"
	exit 0 ;;
fingerprint)
	[ -f "${2:-}" ] || die "usage: $0 fingerprint FILE"
	fingerprint "$2"
	exit 0 ;;
sign) shift ;;
*) die "usage: $0 new-key FILE | fingerprint FILE | sign --key FILE --spec FILE --repo OWNER/REPO --tag vX.Y.Z [...]" ;;
esac

key="" spec="" repo="" tag="" tarball="" prerelease="" dry_run="" out=""
while [ $# -gt 0 ]; do
	case "$1" in
	--key) key="$2"; shift ;;
	--spec) spec="$2"; shift ;;
	--repo) repo="$2"; shift ;;
	--tag) tag="$2"; shift ;;
	--tarball) tarball="$2"; shift ;;
	--prerelease) prerelease=1 ;;
	--dry-run) dry_run=1 ;;
	--out) out="$2"; shift ;;
	*) die "unknown option $1" ;;
	esac
	shift
done
[ -f "${key}" ] && [ -f "${spec}" ] && [ -n "${repo}" ] && [ -n "${tag}" ] ||
	die "sign needs --key, --spec, --repo and --tag"

work="$(mktemp -d)"
trap 'rm -rf "${work}"' EXIT

asset="$(python3 -I -c 'import json,sys; print(json.load(open(sys.argv[1]))["asset"])' "${spec}")"
url="https://github.com/${repo}/releases/download/${tag}/${asset}"
if [ -z "${tarball}" ]; then
	echo "Downloading ${url}"
	curl -fsSL -o "${work}/app.tgz" "${url}"
	tarball="${work}/app.tgz"
fi
sha256="$(sha256sum "${tarball}" | cut -d' ' -f1)"
public_key="$(raw_pub "${key}" | base64)"

python3 -I - "${spec}" "${work}/box-app.json" "${tag#v}" "${url}" "${sha256}" "${public_key}" <<'PY'
import json, sys
spec_path, out, version, url, sha256, public_key = sys.argv[1:]
spec = json.load(open(spec_path))
manifest = {"schema": 1, "name": spec["name"], "version": version, "url": url, "sha256": sha256}
for field in ("command", "env", "secrets", "first_login"):
    if spec.get(field):
        manifest[field] = spec[field]
manifest["public_key"] = public_key
with open(out, "w") as f:
    json.dump(manifest, f, indent=2)
    f.write("\n")
PY
openssl pkeyutl -sign -inkey "${key}" -rawin -in "${work}/box-app.json" -out "${work}/box-app.json.sig"
openssl pkey -in "${key}" -pubout -out "${work}/pub.pem"
openssl pkeyutl -verify -pubin -inkey "${work}/pub.pem" -rawin \
	-in "${work}/box-app.json" -sigfile "${work}/box-app.json.sig" >/dev/null
cat "${work}/box-app.json"
echo "Key fingerprint: $(fingerprint "${key}")"

if [ -n "${out}" ]; then
	mkdir -p "${out}"
	cp "${work}/box-app.json" "${work}/box-app.json.sig" "${out}/"
fi
if [ -n "${dry_run}" ]; then
	echo "Dry run: not uploaded${out:+ (files in ${out})}"
	exit 0
fi

gh release upload "${tag}" -R "${repo}" --clobber "${work}/box-app.json" "${work}/box-app.json.sig"
echo "Attached box-app.json to ${repo} ${tag}"
if [ -n "${prerelease}" ]; then
	if gh release view box-testing -R "${repo}" >/dev/null 2>&1; then
		gh release upload box-testing -R "${repo}" --clobber "${work}/box-app.json" "${work}/box-app.json.sig"
	else
		gh release create box-testing -R "${repo}" --prerelease --title "Newest test version for boxes (opt-in)" \
			--notes "Boxes set to get test versions early read this box-app.json; it points at the newest test release." \
			"${work}/box-app.json" "${work}/box-app.json.sig"
	fi
	echo "Pointed ${repo}'s box-testing release at ${tag}"
fi
