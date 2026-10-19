# Shared by the tailscale-* helpers. Sourced, not run.
#
# Tailscale comes in two places: the copy built into this OS image, and
# releases activated on /data without an OS update:
#   /usr/lib/box/tailscale/{tailscale,tailscaled,VERSION}      built in
#   /data/tailscale/releases/<version>/{tailscale,tailscaled,VERSION}
#   /data/tailscale/current -> releases/<version>               activated
# The newer of "built in" and "current" runs, so an OS update that ships a
# newer Tailscale takes over from an older one activated earlier.

TS_BUILTIN=/usr/lib/box/tailscale
TS_DATA=/data/tailscale
TS_RELEASES=${TS_DATA}/releases
TS_CURRENT=${TS_DATA}/current

# ts_newer A B: true if version A sorts after version B (dotted numbers)
ts_newer() {
	[ "$1" != "$2" ] &&
		[ "$(printf '%s\n%s\n' "$1" "$2" | sort -t. -k1,1n -k2,2n -k3,3n -k4,4n | tail -n 1)" = "$1" ]
}

# ts_complete DIR: true if DIR holds a runnable release
ts_complete() {
	[ -x "$1/tailscale" ] && [ -x "$1/tailscaled" ] && [ -s "$1/VERSION" ]
}

# ts_active_dir: the release to run
ts_active_dir() {
	if ts_complete "${TS_CURRENT}" &&
		ts_newer "$(cat "${TS_CURRENT}/VERSION")" "$(cat "${TS_BUILTIN}/VERSION")"; then
		echo "${TS_CURRENT}"
	else
		echo "${TS_BUILTIN}"
	fi
}

# ts_daemon_version: version reported by the running tailscaled, or nothing
ts_daemon_version() {
	cli="$(ts_active_dir)/tailscale"
	# Prints "Client: <v>" ... "Daemon: <v>-t<commit>-g<commit>"
	v="$(timeout 10 "${cli}" version --daemon 2>/dev/null | sed -n 's/^Daemon: *\([0-9.]*\).*/\1/p')"
	[ -n "${v}" ] || v="$(timeout 10 "${cli}" status --json 2>/dev/null |
		sed -n 's/^ *"Version": *"\([0-9.]*\).*/\1/p' | head -n 1)"
	echo "${v}"
}

# ts_uptime: whole seconds since boot (unaffected by clock changes)
ts_uptime() {
	cut -d. -f1 /proc/uptime
}

# ts_wait_daemon VERSION SECONDS: wait for tailscaled to answer as VERSION
ts_wait_daemon() {
	deadline=$(($(ts_uptime) + $2))
	while [ "$(ts_uptime)" -lt "${deadline}" ]; do
		# Only ask the CLI once the service is up; it's slow to give up otherwise
		if systemctl is-active -q tailscaled && [ "$(ts_daemon_version)" = "$1" ]; then
			return 0
		fi
		sleep 2
	done
	return 1
}
