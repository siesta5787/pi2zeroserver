#!/bin/bash
# Set (or change) the Wi-Fi network on a flashed SD card, from a Linux PC.
# Stopgap for dev cards until the setup hotspot exists.
#
# 1. Plug the flashed card in; the desktop mounts its "rootfs" partition
#    (usually at /media/$USER/rootfs).
# 2. sudo tools/set-wifi.sh /media/$USER/rootfs
# 3. Enter the network name and password when asked, then eject the card.
#
# Running it again with the same network name replaces the password.
set -euo pipefail

ROOT="${1:-}"
if [ -z "${ROOT}" ] || [ ! -f "${ROOT}/etc/iwd/main.conf" ]; then
	echo "Usage: sudo $0 <mounted rootfs partition of the SD card>" >&2
	echo "(the folder must contain etc/iwd/main.conf)" >&2
	exit 1
fi
if [ "$(id -u)" -ne 0 ] && [ -z "${SET_WIFI_NO_ROOT:-}" ]; then
	echo "Run with sudo: the card's files belong to root." >&2
	exit 1
fi

read -r -p "Wi-Fi network name: " ssid
read -r -s -p "Wi-Fi password: " password
echo
[ -n "${ssid}" ] || { echo "Network name can't be empty." >&2; exit 1; }
[ "${#password}" -ge 8 ] || { echo "WPA passwords are at least 8 characters." >&2; exit 1; }

# iwd names the file after the SSID, hex-encoded if it has unusual characters
if [[ "${ssid}" =~ ^[A-Za-z0-9_\ -]+$ ]]; then
	name="${ssid}"
else
	name="=$(printf '%s' "${ssid}" | od -An -tx1 | tr -d ' \n')"
fi

dir="${ROOT}/var/lib/iwd"
install -d -m 0700 "${dir}"
umask 077
printf '[Security]\nPassphrase=%s\n' "${password}" > "${dir}/${name}.psk"
sync
echo "Saved Wi-Fi \"${ssid}\". Eject the card and put it in the Pi."
