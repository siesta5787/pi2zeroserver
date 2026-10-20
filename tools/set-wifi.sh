#!/bin/bash
# Set (or change) the Wi-Fi network on a flashed SD card, from a Linux PC.
# For dev cards (or if you'd rather not use the box's setup hotspot).
#
# 1. Plug the flashed card in; the desktop mounts its "data" partition
#    (usually at /media/$USER/data).
# 2. sudo tools/set-wifi.sh /media/$USER/data
# 3. Enter the network name and password when asked, then eject the card.
#
# Running it again with the same network name replaces the password. The
# setting lives on the data partition, so it survives A/B updates.
#
# Non-interactive (used by the build for dev images): set WIFI_SSID and
# WIFI_PASSWORD in the environment.
set -euo pipefail

DATA="${1:-}"
if [ -z "${DATA}" ] || [ ! -d "${DATA}" ]; then
	echo "Usage: sudo $0 <mounted data partition of the SD card>" >&2
	exit 1
fi
if [ -f "${DATA}/etc/iwd/main.conf" ]; then
	echo "That's a system partition. Use the partition named \"data\" instead." >&2
	exit 1
fi
if [ "$(id -u)" -ne 0 ] && [ -z "${SET_WIFI_NO_ROOT:-}" ]; then
	echo "Run with sudo: the card's files belong to root." >&2
	exit 1
fi

ssid="${WIFI_SSID:-}"
password="${WIFI_PASSWORD:-}"
if [ -z "${ssid}" ]; then
	read -r -p "Wi-Fi network name: " ssid
	read -r -s -p "Wi-Fi password: " password
	echo
fi
[ -n "${ssid}" ] || { echo "Network name can't be empty." >&2; exit 1; }
[ "${#password}" -ge 8 ] && [ "${#password}" -le 63 ] ||
	{ echo "WPA passwords have 8 to 63 characters." >&2; exit 1; }

# iwd names the file after the SSID, hex-encoded if it has unusual characters
if [[ "${ssid}" =~ ^[A-Za-z0-9_\ -]+$ ]]; then
	name="${ssid}"
else
	name="=$(printf '%s' "${ssid}" | od -An -tx1 | tr -d ' \n')"
fi

install -d -m 0700 "${DATA}/iwd"
umask 077
# iwd's settings format: backslashes doubled, leading spaces written as \s
value="${password//\\/\\\\}"
lead="${value%%[! ]*}"
value="${lead// /\\s}${value#"${lead}"}"
printf '[Security]\nPassphrase=%s\n' "${value}" > "${DATA}/iwd/${name}.psk"
sync
echo "Saved Wi-Fi \"${ssid}\"."
