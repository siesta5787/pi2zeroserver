#!/bin/bash
# Makes a preinstalled image: the generic box image with an app already chosen
# and a copy of its release on the card, so the box runs the app from its first
# boot, without internet. Other than that it's the generic image: system
# updates still come from the box's releases, app updates from the app's.
#
#   tools/make-app-image.sh BASE.img[.xz] OUT.img[.xz] APP-LINK
#                           [--fingerprint "abcd ef01 ..."] [--manifest box-app.json --tarball FILE]
#
#   APP-LINK       github.com/owner/repo (or the https link to a box-app.json)
#   --fingerprint  refuse unless the app's key has this fingerprint
#   --manifest, --tarball  use these files (the release's box-app.json, with
#                  its .sig next to it, and the asset) instead of downloading
#
# The box pins the app (and its publisher key) as if chosen on the setup page:
# it only installs that app's releases signed with that key, and changing apps
# takes a reflash. Needs: python3, openssl, curl, sfdisk, debugfs (e2fsprogs), xz.
set -euo pipefail

die() { echo "$*" >&2; exit 1; }
[ $# -ge 3 ] || die "usage: $0 BASE.img[.xz] OUT.img[.xz] APP-LINK [--fingerprint F] [--manifest FILE --tarball FILE]"
base="$1" out="$2" link="$3"
shift 3
want_fp="" manifest="" tarball=""
while [ $# -gt 0 ]; do
	case "$1" in
	--fingerprint) want_fp="$2"; shift ;;
	--manifest) manifest="$2"; shift ;;
	--tarball) tarball="$2"; shift ;;
	*) die "unknown option $1" ;;
	esac
	shift
done
[ -f "${base}" ] || die "no image at ${base}"

work="$(mktemp -d)"
trap 'rm -rf "${work}"' EXIT

# The same link rules as the box's setup page (agent/src/app_source.rs)
read -r shown manifest_url testing_url < <(python3 -I - "${link}" <<'PY'
import re, sys
link = sys.argv[1].strip().rstrip("/")
bare = re.sub(r"^https?://", "", link)
m = re.match(r"^(?:www\.)?github\.com/([A-Za-z0-9_.-]+)/([A-Za-z0-9_.-]+)", bare)
if m and not m.group(1).startswith(".") and not m.group(2).startswith("."):
    owner, repo = m.group(1), m.group(2).removesuffix(".git")
    base = f"https://github.com/{owner}/{repo}/releases"
    print(f"github.com/{owner}/{repo}", f"{base}/latest/download/box-app.json",
          f"{base}/download/box-testing/box-app.json")
elif link.startswith("https://") and link.endswith(".json") and " " not in link:
    print(link, link, "-")
else:
    sys.exit("not an app link: github.com/owner/repo or https://.../box-app.json")
PY
)

if [ -n "${manifest}" ]; then
	cp "${manifest}" "${work}/box-app.json"
	cp "${manifest}.sig" "${work}/box-app.json.sig"
else
	curl -fsSL -o "${work}/box-app.json" "${manifest_url}"
	curl -fsSL -o "${work}/box-app.json.sig" "${manifest_url}.sig"
fi

# Check the signature with the key the release names (what the box pins)
python3 -I - "${work}" <<'PY'
import base64, json, sys
work = sys.argv[1]
m = json.load(open(f"{work}/box-app.json"))
key = base64.b64decode(m["public_key"])
assert len(key) == 32, "the app's key isn't an Ed25519 key"
# DER SubjectPublicKeyInfo for a raw Ed25519 key
open(f"{work}/pub.der", "wb").write(bytes.fromhex("302a300506032b6570032100") + key)
for field in ("name", "version", "url", "sha256"):
    open(f"{work}/{field}", "w").write(str(m[field]))
open(f"{work}/public_key", "w").write(m["public_key"])
PY
openssl pkeyutl -verify -pubin -keyform DER -inkey "${work}/pub.der" -rawin \
	-in "${work}/box-app.json" -sigfile "${work}/box-app.json.sig" >/dev/null ||
	die "box-app.json isn't signed by the key it names"
fp="$(tail -c 32 "${work}/pub.der" | sha256sum | cut -c1-20 | sed 's/..../& /g; s/ $//')"
if [ -n "${want_fp}" ] && [ "${want_fp}" != "${fp}" ]; then
	die "the app's key fingerprint is ${fp}, not ${want_fp}"
fi
name="$(cat "${work}/name")" version="$(cat "${work}/version")"

if [ -n "${tarball}" ]; then
	cp "${tarball}" "${work}/app.tgz"
else
	curl -fsSL -o "${work}/app.tgz" "$(cat "${work}/url")"
fi
[ "$(sha256sum "${work}/app.tgz" | cut -d' ' -f1)" = "$(cat "${work}/sha256")" ] ||
	die "the app's download doesn't match its box-app.json"

python3 -I - "${work}/app-source.json" "${shown}" "${manifest_url}" "${testing_url}" "$(cat "${work}/public_key")" "${name}" <<'PY'
import json, sys
out, link, url, testing, key, name = sys.argv[1:]
source = {"link": link, "manifest_url": url, "public_key": key, "name": name}
if testing != "-":
    source["testing_url"] = testing
json.dump(source, open(out, "w"), indent=2)
PY

# The card image, and its data partition (the 4th)
case "${base}" in
*.xz) xz -dc "${base}" > "${work}/card.img" ;;
*) cp --sparse=always "${base}" "${work}/card.img" ;;
esac
read -r start size < <(sfdisk -J "${work}/card.img" | python3 -I -c \
	'import json,sys; p=json.load(sys.stdin)["partitiontable"]["partitions"][3]; print(p["start"]*512, p["size"]*512)')
dd if="${work}/card.img" of="${work}/data.ext4" bs=1M iflag=skip_bytes,count_bytes skip="${start}" count="${size}" status=none
cat > "${work}/debugfs.cmds" <<CMDS
mkdir /box
write ${work}/app-source.json /box/app-source.json
mkdir /app
mkdir /app/preload
write ${work}/box-app.json /app/preload/box-app.json
write ${work}/box-app.json.sig /app/preload/box-app.json.sig
write ${work}/app.tgz /app/preload/app.tgz
CMDS
for f in /box /box/app-source.json /app /app/preload /app/preload/box-app.json \
	/app/preload/box-app.json.sig /app/preload/app.tgz; do
	printf 'set_inode_field %s uid 0\nset_inode_field %s gid 0\n' "$f" "$f" >> "${work}/debugfs.cmds"
done
debugfs -w -f "${work}/debugfs.cmds" "${work}/data.ext4" >/dev/null 2>"${work}/debugfs.log" || true
debugfs -R "ls -l /app/preload" "${work}/data.ext4" 2>/dev/null | grep -q app.tgz ||
	die "couldn't write to the image's data partition: $(cat "${work}/debugfs.log")"
dd if="${work}/data.ext4" of="${work}/card.img" bs=1M oflag=seek_bytes conv=notrunc seek="${start}" status=none

case "${out}" in
*.xz) xz -T0 -c "${work}/card.img" > "${out}.part" ;;
*) cp --sparse=always "${work}/card.img" "${out}.part" ;;
esac
mv "${out}.part" "${out}"
echo "Wrote ${out}: ${name} ${version} preinstalled, from ${shown} (key ${fp})"
