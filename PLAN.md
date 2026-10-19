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

Tailscale tarball verification (worked out on 1.104.1): each
`tailscale_<v>_arm64.tgz` has a `.sig` that is a raw Ed25519 signature over
`BLAKE2s-256(file) || uint64_le(file length)`, by a key in
`https://pkgs.tailscale.com/distsign.pub`. That key list is itself signed
(`distsign.pub.sig`) by Tailscale's root keys, which the agent should pin.
Latest version: `https://pkgs.tailscale.com/stable/?mode=json` →
`TarballsVersion`. The agent unpacks a verified release to
`/data/tailscale/releases/<v>/` and calls `/usr/libexec/box/tailscale-activate <v>`
(exit 0 activated, 1 rolled back, 2 unusable).

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
   cards via `local/wifi.env` or `sudo tools/set-wifi.sh <mounted data partition>`.)
2. ✅ A/B: genimage, U-Boot bootcount script, RAUC, mark-good. Passes
   `tools/qemu-ab-test.py` (boot A → install to B → forced rollback to A).
   Verified on a Zero 2 W: streamed install over Wi-Fi, booted slot B.
   Still to do on real hardware: power loss mid-update.
3. ✅ `/data`: grows on first boot, read-only rootfs, `/var` in RAM; Wi-Fi
   (iwd), RAUC status and timesyncd's clock persist on `/data` via bind mounts.
   QEMU test covers first-boot growth, a power cut, and `/data` shared by both slots.
4. ✅ Tailscale: official 1.104.1 static binaries built in, state on `/data`,
   newer-wins launcher, `tailscale-activate` with health check and rollback,
   and a boot health check so a broken Tailscale blocks marking an OS slot good.
   QEMU test covers activate, broken-release rollback, and surviving an OS update.
   Downloading/verifying releases is the agent's job (step 5).
5. Agent: proxy, status, manifest, OS/app/Tailscale update flows.
   - ✅ 5a: `agent/` (Rust, built by Buildroot's cargo-package with Rust 1.88):
     port 80, `/_system/` status page + `/_system/api/{health,status}`, reverse
     proxy to 127.0.0.1:8080 with a friendly page when the app is down, boot
     health check. 1.3 MB binary.
   - ✅ 5b: admin password (Argon2id in /data/agent), sessions in an HttpOnly
     SameSite=Strict cookie, per-address lockout, Origin check on posts. Only
     /_system/api/health is public; localhost gets no special trust (tailscale
     serve forwards from 127.0.0.1).
   - ✅ 5d: OS install from the agent: runs `rauc install` (RAUC's service does
     the work) and follows its progress; status page shows progress (self-
     refreshing, no JS), "installed — Restart now", or the error. Sources:
     http(s) URLs or *.raucb files under /data.
   - 5c: signed update manifest + "update available" (needs: where releases
     are hosted); 5e: Tailscale download + distsign verify + activate;
     5f: app releases with activate/rollback
6. Onboarding: AP, captive portal, Tailscale QR.
7. Hardening: firewall, sandboxing, release defconfig (no curl CLI, no HDMI
   console login, release signing key).
8. Releases: CI pipeline, signed manifest, Imager repo, novice guide.
