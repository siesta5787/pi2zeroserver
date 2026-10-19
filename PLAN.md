# Pi Zero 2 W Appliance — Plan

A tiny, out-of-the-way Pi Zero 2 W that runs a static musl Rust web server plus
Tailscale, and that a non-technical person can keep updated from a web page — no
reflashing, no terminal. Flashing the SD card is the hardest step.

## Summary

- **OS:** Buildroot (2026.02 LTS), 64-bit (`raspberrypizero2w_64_defconfig` base),
  systemd, iwd, read-only root filesystem.
- **A/B updates:** RAUC + U-Boot with a boot counter. A bad OS update rolls back
  on its own.
- **App + Tailscale** live on `/data` and update independently of the OS, each with
  its own health check and rollback. A known-good fallback copy ships in the rootfs.
- **Agent** (static Rust, in the read-only rootfs) is the front door on :80. It serves
  `/_system/*` (setup, updates, Wi-Fi, status) and proxies everything else to the app.
- **Setup:** flash → power on → join `Setup-XXXX` from a phone → pick Wi-Fi, set
  password → "Connect Tailscale" (QR code).
- **Security:** only the web port is open, no SSH in release builds, no default
  passwords, everything signed.

## Key decisions

| Decision | Choice | Why |
|---|---|---|
| Update framework | RAUC | Included in Buildroot, signed bundles, streams straight to the spare slot (512 MB RAM), D-Bus API |
| Bootloader | U-Boot + bootcount | Proven A/B on the Zero 2 W; env stored redundantly in the raw gap |
| Init | systemd | Watchdog, restart, sandboxing, timesyncd (no RTC), networkd, mDNS via resolved |
| Wi-Fi | iwd | Small, D-Bus, built-in AP mode; ship both 43436 and 43436s firmware |
| Rootfs | ext4, mounted read-only | U-Boot can read it; survives power loss |
| App/Tailscale | `/data`, fallback in rootfs | Updates without full OS images; static binaries don't depend on the OS |

## SD card layout

```
┌──────────┬──────────┬───────────┬───────────┬────────────────────────┐
│ raw gap  │ p1 boot  │ p2 rootA  │ p3 rootB  │ p4 data (grows to fill)│
│ uboot env│ FAT 64M  │ ext4 512M │ ext4 512M │ ext4                   │
│ (x2)     │ firmware,│ kernel,   │ (spare)   │ wifi, tailscale state, │
│          │ u-boot   │ agent     │           │ app releases, settings │
└──────────┴──────────┴───────────┴───────────┴────────────────────────┘
```

The kernel lives in each root slot (A/B protected). The DT comes from the Pi
firmware on p1, so `config.txt` overlays keep working. p1 is not A/B.

## How the pieces run

```
Browser ──(LAN http://box.local  or  https://box.<tailnet>.ts.net via tailscale serve)──►
  agent  (rootfs; only changes via A/B OS updates)
   ├─ /_system/*   setup · updates · Wi-Fi · status · password
   ├─ /*           proxied to the app on 127.0.0.1:8080
   ├─ RAUC  (D-Bus)       OS install, mark-good
   ├─ iwd   (D-Bus)       Wi-Fi scan/join, setup AP
   ├─ tailscale CLI --json  login link, status, version swap
   └─ health checks + rollback for all three
app        /data/app/current → releases/<ver>/   (unprivileged, sandboxed, 127.0.0.1 only)
tailscaled /data/tailscale/current → <ver>/      (state in /data/tailscale/state)
```

The updater is separate from the app so a bad app update can't break updating.

## Updates

A signed (ed25519) manifest lists the latest OS bundle and app release. Tailscale
comes from `pkgs.tailscale.com` with checksum/signature verification. Public keys
are baked into the read-only rootfs.

| What | How | Health check | On failure |
|---|---|---|---|
| OS | `rauc install <url>` (streamed) → reboot | agent + network + app `/healthz` → mark-good | U-Boot runs out of tries, boots the old slot |
| App | download → verify → switch `current` → restart | `/healthz` OK within 30 s | switch back |
| Tailscale | official static arm64 tarball → verify → switch → restart | backend `Running` within 60 s | switch back, else rootfs copy |

The device checks every ~12 h with jitter. By default OS and Tailscale updates
install automatically overnight; app updates are notify-only. Downgrades need an
explicit choice.

## First-time setup

1. Flash the `.img.xz` with Raspberry Pi Imager (custom repo JSON so it's listed).
2. No Wi-Fi configured → open, time-limited `Setup-XXXX` AP with a captive portal.
3. Pick Wi-Fi, set an admin password and a name → device joins the home Wi-Fi.
4. Page shows `http://box-xxxx.local` and "Connect Tailscale" (QR code login link).
5. Optional `setup.txt` on the boot partition: read once, moved to `/data`, deleted.

Can't reach Wi-Fi for ~10 min → setup AP comes back while it keeps retrying.
Factory reset: UI button, or `factory-reset.txt` on the boot partition.

## Security

- nftables: inbound only TCP 80 from the local subnet, mDNS, and Tailscale. Toggle
  for "Tailscale-only".
- Tailscale: no subnet router, no exit node, no accept-routes.
- No SSH or shell services, root locked. The dev build adds SSH (Tailscale only, keys only).
- Bluetooth and HDMI off.
- App runs as an unprivileged user with systemd sandboxing, bound to 127.0.0.1.
- Agent: argon2 passwords, rate-limited login, CSRF protection. rustls, no OpenSSL.
- Signed updates; security updates automatic by default.
- Known limit: no secure boot on the Zero 2, so a physical attacker can change the SD card.

## Repo layout

Buildroot can't build in a path with spaces, so this project folder holds the
source (in git) and a sibling folder on the same drive holds the build:

```
RPi Server DIstro/          (this folder, git)
  build.sh                  build wrapper: ./build.sh [dev|release] [make targets]
  br2-external/             board/zero2w/{genimage.cfg, config.txt, cmdline.txt,
                            linux.fragment, post-build.sh, post-image.sh,
                            rootfs-overlay/, rootfs-overlay-dev/}
                            configs/{box_dev_defconfig, box_release_defconfig}
                            package/{agent, tailscale-static, app-baseline}
  agent/                    Rust (axum, zbus, rustls, ed25519), aarch64-unknown-linux-musl
  tools/                    make-manifest, sign, imager-repo.json
  local/                    gitignored: wifi.env, authorized_keys (dev builds only)

/media/me/Claude/rpi-build/ (build tree, no spaces)
  buildroot/                Buildroot 2026.02.3, fetched by build.sh
  br2-external ->           symlink to the project's br2-external/
  output/<variant>/         one build tree per variant; images in images/
  cache/dl/                 downloaded sources
  cache/ccache/             compiler cache
```

## Build caching

- `cache/dl/` — downloaded sources (`BR2_DL_DIR`), shared by all variants.
- `cache/ccache/` — compiler cache (`BR2_CCACHE`, 20 GB), so rebuilds after a
  clean or config change are fast. Also covers the kernel.
- `output/<variant>/` — out-of-tree builds; dev and release don't clobber each other.
- `BR2_PER_PACKAGE_DIRECTORIES` — lets top-level `make -j` build packages in parallel.
- Prebuilt Bootlin toolchain — no cross-compiler build at all.
- `cache/cargo/` — Cargo registry + target cache for the agent (later).

## Work order

1. ✅ Boots on Wi-Fi: dev defconfig, systemd, iwd. (SSH dropped; Wi-Fi for dev
   cards via `local/wifi.env` or `sudo tools/set-wifi.sh <mounted rootfs>`.)
2. ✅ A/B: genimage, U-Boot bootcount script, RAUC, mark-good. Passes
   `tools/qemu-ab-test.py` (boot A → install to B → forced rollback to A).
   Still to do on real hardware: power loss mid-update.
3. `/data`: grows on first boot, persistent `/etc` bits, read-only rootfs.
4. Tailscale: rootfs copy, state on `/data`, swap/rollback updater.
5. Agent: proxy, status, manifest, OS/app/Tailscale update flows.
6. Onboarding: AP, captive portal, Tailscale QR.
7. Hardening: firewall, sandboxing, release defconfig without SSH.
8. Releases: CI pipeline, signed manifest, Imager repo, novice guide.
