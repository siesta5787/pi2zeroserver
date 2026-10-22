#!/bin/bash
# Creates the release signing keys, outside this repo, and points the build at them.
#
#   tools/make-release-keys.sh [folder]     (default: ~/box-release-keys)
#
# Makes:
#   rauc-key.pem, rauc-cert.pem   signs OS update bundles (devices trust the cert)
#   manifest-key.pem              signs the release manifest (devices trust its public half)
# and writes local/release.env (gitignored) with their paths, which build.sh and
# tools/publish-release.sh read for the release variant.
#
# Back the folder up somewhere safe and offline (e.g. two USB sticks, or an
# encrypted backup): boxes built with these keys only accept updates signed by
# them. Lose them and every release box needs reflashing; leak them and someone
# else could sign updates your boxes accept.
set -euo pipefail

PROJECT_DIR="$(cd "$(dirname "$0")/.." && pwd)"
DIR="${1:-${HOME}/box-release-keys}"
case "$(realpath -m "${DIR}")/" in
"${PROJECT_DIR}"/*) echo "Keep release keys outside the repo (not under ${PROJECT_DIR})" >&2; exit 1 ;;
esac
if [ -e "${DIR}/rauc-key.pem" ] || [ -e "${DIR}/manifest-key.pem" ]; then
	echo "${DIR} already has release keys; not overwriting them." >&2
	exit 1
fi

install -d -m 0700 "${DIR}"
umask 077
# 100 years: devices check the certificate's dates against their clock
openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:prime256v1 -nodes -days 36500 \
	-subj "/O=Box/CN=Box release update signing" \
	-keyout "${DIR}/rauc-key.pem" -out "${DIR}/rauc-cert.pem" 2>/dev/null
openssl genpkey -algorithm ed25519 -out "${DIR}/manifest-key.pem"
chmod 0400 "${DIR}"/*.pem

mkdir -p "${PROJECT_DIR}/local"
cat > "${PROJECT_DIR}/local/release.env" <<ENV
# Release signing keys (made by tools/make-release-keys.sh). Paths only.
BOX_RAUC_KEY="${DIR}/rauc-key.pem"
BOX_RAUC_CERT="${DIR}/rauc-cert.pem"
BOX_MANIFEST_KEY="${DIR}/manifest-key.pem"
ENV
echo "Release keys are in ${DIR}; local/release.env points the build at them."
echo "Back that folder up now (offline). ./build.sh release builds with them."
