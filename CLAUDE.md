# CLAUDE.md

Buildroot-based A/B-updating appliance OS for the Raspberry Pi Zero 2 W: runs a
static musl Rust web server (kid-phone-server, a separate repo) plus Tailscale,
kept up to date by a non-technical user from a web page. Design, decisions and
step status: `PLAN.md`. The app's side of the integration: `docs/box-api.md`.

## How the box fits together

- `agent/` (Rust, `box-agent`, in the read-only image) is the front door:
  - port 80 on the home LAN: `/_system/` only (setup, status, updates, behind the
    admin password); other paths say "open the app over Tailscale". Port 80 also
    answers over Tailscale (`http://box/` via MagicDNS, handy because phones
    can't resolve box.local while their Tailscale is on); once HTTPS works,
    those visitors are redirected to `https://box.<tailnet>.ts.net/...`;
  - `127.0.0.1:8081`, the tailnet front: `/_system/` plus a reverse proxy to the
    app (127.0.0.1:3100) with `X-Forwarded-Proto: https`. The agent publishes it
    with `tailscale serve --bg --https=443` once Tailscale is connected;
  - `/run/box/api.sock` (root:app 0660): the box API for the app.
  Only `/_system/api/health` is public; localhost gets no special trust.
- Wi-Fi (`agent/src/wifi.rs`): the agent drives iwd with `busctl --json` (no
  D-Bus crate). Joining writes the password as an iwd network file in
  `/data/iwd` and calls `Network.Connect` (no iwd agent). With no saved network,
  or offline for 3 min, it starts `box-hotspot.service` (hostapd, config from
  `/usr/libexec/box/hotspot-config`): `Box-Setup-XXXX`, password `boxsetup`. The
  unit has `Conflicts=iwd.service`, so the hotspot and iwd never run together;
  the agent leaves the hotspot by starting iwd, and calls busctl with
  `--auto-start=no` so D-Bus activation can't start iwd behind its back.
  networkd gives the hotspot 192.168.4.1 and DHCP (`10-setup-hotspot.network`,
  matched by `WLANInterfaceType=ap`); `captive_dns.rs` answers every name with
  192.168.4.1 and the LAN front redirects non-`/_system` requests there. Hotspot
  start/fail messages also go to the kernel log, i.e. the Pi's screen.
- Recovery: `box-recovery.service` (`/usr/libexec/box/recovery`) runs before
  anything uses /data. `reset-password*` / `factory-reset*` files on the boot
  partition (`/dev/mmcblk0p1`, FAT), or `/data/.factory-reset` from the page's
  "Erase everything" button, delete `/data/agent/admin-password` or
  `/data/{agent,iwd,tailscale,app}`; tmpfiles recreates the folders. Keep that
  list in sync when adding user state on /data (rauc and timesync are system
  state and stay).
- "Connect Tailscale" (`tailscale_login.rs`) runs `tailscale up` and shows the
  sign-in link and a QR code (qrcode crate, SVG).
- State on `/data`: `agent/` (admin password), `iwd/`, `tailscale/{state,releases,
  current}`, `rauc/`, `timesync/`, `app/{releases,current,state,run,env,snapshots}`.
- Activation scripts in `/usr/libexec/box/` do the switch-and-health-check for
  things updated on `/data`; the agent downloads and verifies, then calls them.
  Exit status 0 activated, 1 failed and rolled back, 2 release unusable:
  `tailscale-activate <v>`, `app-activate <v>` (snapshots and restores the app's
  SQLite files too, since a newer app may migrate the DB).
- Trust: OS bundles are checked by RAUC (`local/rauc` key), the release manifest
  by the agent (`local/manifest` key, pinned in `/etc/box/manifest-key.pub`), the
  app by the manifest's SHA-256, Tailscale by its distsign chain from the root
  keys in `agent/src/tailscale-roots/`.

## Building

- Always build with `./build.sh [variant] [buildroot targets...]` (variant
  defaults to `dev`). Never run Buildroot's `make` by hand from this folder:
  Buildroot breaks on paths with spaces, so the Buildroot tree, outputs and
  caches live in `/media/me/Claude/rpi-build/` (`buildroot/`, `output/<variant>/`,
  `cache/{dl,ccache}`, and a `br2-external` symlink back to this repo).
- Images land in `/media/me/Claude/rpi-build/output/dev/images/`
  (`sdcard.img[.xz]` to flash, `box-<version>.raucb` signed update).
- Builds longer than ~1 hour: launch fully detached so tool time limits can't
  kill them, then wait on the exit file:
  `setsid nohup bash -c './build.sh dev > <out>/build.log 2>&1; echo $? > <out>/build.exit' </dev/null >/dev/null 2>&1 &`
  Check no other make is running on the same output dir first.
- What each kind of change needs (Buildroot doesn't notice most of these):
  - edited `configs/box_*_defconfig` → `./build.sh dev box_dev_defconfig` (reloads
    it; `olddefconfig` does not), plus `<pkg>-dirclean` for packages you removed
  - `linux.fragment` → `./build.sh dev linux-reconfigure`
  - `uboot.fragment` or `patches/uboot/` → `./build.sh dev uboot-dirclean`
  - `busybox.fragment` → `./build.sh dev busybox-dirclean` (BusyBox lacks many
    applets by default, e.g. `timeout`, `stat`; check before using one in a script)
  - `config.txt` → `./build.sh dev rpi-firmware-rebuild`
  - `boot.cmd` → `./build.sh dev host-uboot-tools-rebuild` (boot.scr is compiled
    when host-uboot-tools builds, not on every build)
  - rootfs overlay, `post-build.sh`, `post-image.sh` → plain `./build.sh dev`
  - changed a package's `_DEPENDENCIES` → `<pkg>-dirclean` (`-rebuild` doesn't
    refresh its per-package sysroot, so new libraries stay invisible)
  - `agent/` sources → `./build.sh dev box-agent-rebuild` and then `./build.sh dev`
    (local package: Buildroot re-copies it only on rebuild, and `-rebuild` alone
    may stop after the copy; check `target/usr/bin/box-agent`'s timestamp). Run `cargo fmt`, `cargo clippy --all-targets`
    and `cargo test` in `agent/` first: the agent targets Rust 1.88 (Buildroot's),
    so keep `rust-version` and let Cargo pick compatible crate versions.
- The project drive is a USB disk. After long builds, check
  `journalctl -k --since <build start> | grep -E "sdc|usb .*reset|I/O error"`.
  If writes failed, dirclean the packages built in that window: they can
  contain silently zero-filled files.

## Testing

- `tools/qemu-ab-test.py` boots the image in QEMU (raspi3b, same SoC) and runs
  ~60 checks: first boot on slot A (marked good, read-only root, /data grown),
  agent login and APIs, Tailscale activate/rollback, a power cut, the real
  kid-phone-server binary (fetched from GitHub on this machine) with a broken
  update rolled back, "Erase everything" (last), the setup hotspot and Wi-Fi joins on three
  `mac80211_hwsim` radios (box; a test hostapd as the "home" AP; a test iwd as
  the "phone", with a runtime drop-in keeping the system iwd off those two),
  a password reset requested from the boot partition, a signed OS install into slot B through the agent, and
  rollback from a broken B. Run it after anything touching boot, U-Boot, RAUC,
  the kernel, systemd units or the agent. It takes ~25 min: launch it detached
  like a build. Its docstring lists the QEMU-only adaptations (U-Boot image
  header, pinmux, watchdog); keep production files free of QEMU workarounds.
  A failed run keeps its card as `qemu-ab-test/sdcard.failed.img`.
- Watching long runs: the test reads its script once at start, so editing it
  while a run is going is safe once `qemu-ab-test.result` exists. Services
  report "active" before they're listening: poll for the port, don't check once.
  `pgrep -f`/`pkill -f` also match your own command line (`[q]emu` patterns help),
  and a monitor started alongside a launch can read the previous run's files.
- Real device (dev builds): `sudo tools/set-wifi.sh /media/$USER/data` puts
  Wi-Fi on a flashed card's data partition; `tools/serve-updates.py` serves bundles for
  `rauc install http://<pc>:8000/<bundle>` (RAUC streaming needs HTTP Range
  support, which `python3 -m http.server` lacks). The dev device answers at
  `box.local`; it has no SSH (by design).

## Releasing

- Commit first, then build: the version (`YYYY.MM.DD.HHMM-g<commit>`) gets
  `-dirty` from uncommitted changes, and `tools/publish-release.sh` refuses those.
- `tools/publish-release.sh [--app-tag vX.Y.Z] [--dry-run]` publishes the current
  build's OS bundle plus a signed `manifest.json` (also pinning a kid-phone-server
  release by SHA-256) as a GitHub release of this (public) repo. Devices poll
  `releases/latest/download/manifest.json` (+ `.sig`) a minute after boot and
  every ~12 h. Use `--dry-run` to inspect the manifest first. Afterwards, fetch
  the manifest from that URL and verify it against the image's
  `/etc/box/manifest-key.pub`, like a device would.
- Releases so far are signed with the dev keys in `local/`; a release key and a
  release defconfig come with step 7. First published: `v2026.10.20.0316-gcdbc7c3`
  (pins kid-phone-server v0.18.6).
- The app (kid-phone-server, separate repo) is never built here; the manifest
  just pins one of its releases. See PLAN.md "The app".

## Things that bit us

- U-Boot silently drops unknown fragment options, and renames them between
  releases (2026.01: `ENV_REDUNDANT`, `ENV_MMC_DEVICE_INDEX`). `post-image.sh`
  fails the build if any `uboot.fragment` line is missing from U-Boot's
  `.config`. Keep the env settings in sync with `/etc/fw_env.config`.
- RPi kernel device trees name the PL011 `arm,pl011-axi`; upstream U-Boot needs
  `patches/uboot/0001-*` or it crashes before printing anything.
- Kernel modules are xz-compressed: `CONFIG_MODULE_DECOMPRESS=y` is required or
  no module loads (no Wi-Fi).
- Read-only root: anything a service must keep goes on `/data` via a
  `BindPaths=` drop-in plus a `tmpfiles.d/box-data.conf` entry (see iwd,
  timesyncd). Don't write into `/etc` or `/var` expecting it to persist.
- Resizing a partition makes udev re-probe it and briefly drop its by-label
  link; systemd then unmounts anything bound to it. `grow-data` does all
  resizing before `/data` is mounted and waits with `udevadm settle`.
- Don't order units on `network-online.target`: it pulls in
  systemd-networkd-wait-online, which fails after ~2 min whenever there's no
  network (before Wi-Fi setup, in QEMU) and leaves the system degraded. Use
  `network.target` and let services cope without a network.
- The agent must wait for `/data` (`RequiresMountsFor`) and only offers first-time
  password setup while `/data` is mounted; otherwise a slow mount (fsck after a
  power cut) looks like a brand-new box and anyone on the LAN could claim it.
- curl-sys silently builds its own curl without HTTPS when pkg-config can't find
  the system libcurl; `box-agent.mk` refuses to install a binary that doesn't
  link `libcurl.so`.
- Files that outlive OS updates must belong to users with fixed IDs
  (`users.table`, e.g. `app` 950); Buildroot's automatic IDs can shift between
  builds.
- BusyBox `wget` re-POSTs on redirects and hides `Set-Cookie`; the dev image has
  the `curl` command for tests and debugging (to be dropped from release images).
- The tar crate skips archive entries with `..` instead of failing; nothing
  escapes, but don't rely on an error to detect hostile archives.
- iwd: after a failed handshake (wrong password) the next `Network.Connect` on
  that network asks an agent for the password and fails with "No Agent
  registered"; retrying once works. iwd also won't auto-rejoin that network, so
  after a failed join the agent reconnects to the previous one explicitly.
  `Network.Connect` on the network it's already on returns success without
  trying anything, so a new password for the current network needs a
  `Station.Disconnect` first (else a typo gets saved as "joined").
- iwd's access point mode does not work with brcmfmac (START_AP fails with
  -22: iwd runs the AP's association itself, which this FullMAC driver doesn't
  support); hostapd does. The QEMU test's hwsim radios (mac80211) accept both,
  so only the real Pi showed it. The chip also can't scan as an access point:
  the network list comes from a scan just before the hotspot starts. The test
  Zero 2 W has a BCM43430 (brcmfmac43430-sdio), like the Pi 3, not a 43436.
- iwd deletes and recreates wlan interfaces on mac80211 drivers (hwsim) unless
  `[DriverQuirks] DefaultInterface=*` (set in main.conf); without it wlan0 is
  gone while iwd is stopped and hostapd has nothing to run on. brcmfmac keeps
  its interface either way.
- `hostapd_cli` needs `-s <dir>` (its own socket's folder; the default
  /var/run/hostapd doesn't exist): the agent uses /run/box-hotspot for both.
- No RTC: `/usr/lib/clock-epoch` (stamped in `post-build.sh`) keeps the clock at
  least at build time, or offline devices reject bundle signatures.

## Secrets and keys

- `local/` is gitignored: `wifi.env` (dev Wi-Fi), `rauc/dev-{key,cert}.pem` (OS
  bundle signing) and `manifest/dev-key.pem` (release manifest signing), the keys
  generated by `build.sh` on first build. Devices only accept bundles and
  manifests signed by the keys their image was built with; losing them means
  reflashing dev devices. Never commit anything from `local/` except the README
  and example. The repo is public: audit history before pushing anything secret-
  looking.
- Release builds will require `BOX_RAUC_KEY`/`BOX_RAUC_CERT` and `BOX_MANIFEST_KEY`
  from outside the repo.

## Git

- Commit identity is set repo-locally (GitHub noreply address). Remote:
  `github.com/siesta5787/pi2zeroserver` (public since 2026-10-19), branch `main`.
  Releases are published there too. The app lives in
  `github.com/siesta5787/kid-phone-server`; this repo doesn't change it.
