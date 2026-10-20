# CLAUDE.md

Buildroot-based A/B-updating appliance OS for the Raspberry Pi Zero 2 W: runs a
static musl Rust web server plus Tailscale, kept up to date by a non-technical
user from a web page. Design, decisions and step status: `PLAN.md`.

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
  - `agent/` sources → `./build.sh dev box-agent-rebuild` (local package; Buildroot
    re-copies it only on rebuild). Run `cargo test` in `agent/` first: the agent
    targets Rust 1.88 (Buildroot's), so keep `rust-version` and let Cargo pick
    compatible crate versions.
- The project drive is a USB disk. After long builds, check
  `journalctl -k --since <build start> | grep -E "sdc|usb .*reset|I/O error"`.
  If writes failed, dirclean the packages built in that window: they can
  contain silently zero-filled files.

## Testing

- `tools/qemu-ab-test.py` boots the image in QEMU (raspi3b, same SoC) and checks
  first boot on slot A (marked good, read-only root, /data grown), a power cut,
  a signed install into slot B, booting B with the same /data, and rollback
  from a broken B. Run it after anything
  touching boot, U-Boot, RAUC, the kernel or systemd units. It runs ~20 min:
  launch it detached like a build. Its docstring lists the QEMU-only
  adaptations (U-Boot image header, pinmux, watchdog); keep production files
  free of QEMU workarounds.
- Real device (dev builds): `sudo tools/set-wifi.sh /media/$USER/data` puts
  Wi-Fi on a flashed card's data partition; `tools/serve-updates.py` serves bundles for
  `rauc install http://<pc>:8000/<bundle>` (RAUC streaming needs HTTP Range
  support, which `python3 -m http.server` lacks). The dev device answers at
  `box.local`; it has no SSH (by design).

## Releasing

- `tools/publish-release.sh [--app-tag vX.Y.Z] [--dry-run]` publishes the current
  build's OS bundle plus a signed `manifest.json` (also pinning a kid-phone-server
  release by SHA-256) as a GitHub release of this (public) repo. Devices poll
  `releases/latest/download/manifest.json`. Refuses `-dirty` builds unless
  `--allow-dirty`. Use `--dry-run` to inspect the manifest first.
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
- Release builds will require `BOX_RAUC_KEY`/`BOX_RAUC_CERT` from outside the repo.

## Git

- Commit identity is set repo-locally (GitHub noreply address). Remote:
  `github.com/siesta5787/pi2zeroserver` (private), branch `main`.
