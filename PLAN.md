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
- **Setup:** flash → power on → join `Box-Setup-XXXX` (password `boxsetup`) from
  a phone → set the admin password, pick Wi-Fi → "Connect Tailscale" (link + QR code).
- **Security:** only the web port is open, no SSH in release builds, no default
  passwords, everything signed.

## Key decisions

| Decision | Choice | Why |
|---|---|---|
| Update framework | RAUC | Included in Buildroot, signed bundles, streams straight to the spare slot (512 MB RAM), D-Bus API |
| Bootloader | U-Boot + bootcount | Proven A/B on the Zero 2 W; env stored redundantly in the raw gap |
| Init | systemd | Watchdog, restart, sandboxing, timesyncd (no RTC), networkd, mDNS via resolved |
| Wi-Fi | iwd (+ hostapd for the setup hotspot) | Small, D-Bus; iwd's AP mode doesn't work on brcmfmac, so the hotspot uses hostapd |
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
   ├─ iwd   (D-Bus)       Wi-Fi scan/join; hostapd for the setup AP
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
2. No Wi-Fi saved → the agent starts hostapd (`box-hotspot.service`, which stops
   iwd: they can't share the radio, and iwd's own AP mode fails on brcmfmac):
   `Box-Setup-XXXX` (end of the MAC), WPA2 password `boxsetup` (fixed and
   published since the box has no screen or label).
   networkd gives it 192.168.4.1 and DHCP; the agent answers all DNS with
   192.168.4.1 and redirects other web requests to `/_system/`, so phones show
   their "sign in to network" prompt.
3. Set the admin password (first visit), then Wi-Fi: pick a network from the list
   (scanned just before the hotspot started; the Pi's brcmfmac can't scan while
   it's an AP) or type its name. The box leaves the hotspot to join; on failure
   the hotspot comes straight back with the error.
4. Back on the home Wi-Fi: `http://box.local/_system/` → "Connect Tailscale" runs
   `tailscale up` and shows its sign-in link plus a QR code.
5. Later: optional `setup.txt` on the boot partition; a box name (several boxes on
   one network would all be `box.local`).

Offline for 3 min with saved networks → the hotspot comes up for 5 min (15 while
someone is connected), then the box tries its networks again; it leaves early
when a scan sees a saved network.
Recovery (box-recovery.service, early at boot, before anything uses /data): an
empty `reset-password.txt` on the card's BOOT partition (FAT, readable on any
computer) forgets the admin password; `factory-reset.txt` erases Wi-Fi,
Tailscale, the app and its data, and the password (the OS and its backup slot
stay). Names match loosely (case, hidden `.txt`). The page's "Erase everything"
button signs out of Tailscale and leaves `/data/.factory-reset` for the same
service. The sign-in page explains the password reset.

## Security

- nftables: inbound only TCP 80 from the local subnet, mDNS, and Tailscale.
  On the setup hotspot also DHCP (67/udp) and DNS (53/udp). Toggle
  for "Tailscale-only".
- Tailscale: no subnet router, no exit node, no accept-routes.
- No SSH or shell services, root locked. The dev build adds SSH (Tailscale only, keys only).
- Bluetooth and HDMI off.
- App runs as an unprivileged user with systemd sandboxing, bound to 127.0.0.1.
- Agent: argon2 passwords, rate-limited login, CSRF protection. rustls, no OpenSSL.
- Signed updates; security updates automatic by default.
- Known limit: no secure boot on the Zero 2, so a physical attacker can change the SD card.

## The app: kid-phone-server

Decided 2026-10-19. The box runs https://github.com/siesta5787/kid-phone-server
(static musl Rust, listens on 127.0.0.1:3100, keeps state in `data/`).

- **Access: Tailscale HTTPS only.** `tailscale serve` → agent (a second, tailnet-only
  listener) → app, with `X-Forwarded-Proto: https`, so the app's Secure cookies
  work. The home LAN (port 80) only gets the box's own `/_system/` pages; app
  paths there point people at the tailnet address.
- **Releases: pinned by the box manifest.** This repo's signed `manifest.json`
  names the exact app tarball (GitHub release asset) and its SHA-256; the box
  installs nothing else. Publishing a new box manifest is what rolls out an app
  version. The app's own release CI is unchanged.
- **Layout on /data:** `app/releases/<v>/` (binary + `static/`), `app/current` →
  active release, `app/state/` (the app's `data/`: SQLite DB, backups...),
  `app/run/` working dir with `data` → `../state` and `static` → `../current/static`,
  `app/env` (DATABASE_URL, BIND_ADDR, first-run ADMIN_*). Runs as user `app`,
  sandboxed, writable only `app/state`.
- **Updates with rollback:** stop app → snapshot the DB → switch `current` → start →
  healthy (HTTP answer, not 5xx, within 60 s) or restore the snapshot and switch
  back. The snapshot matters: an older binary can't open a DB a newer one migrated.
- **Box API for the app:** a Unix socket only the `app` user can open
  (`/run/box/api.sock`, env `BOX_API_SOCKET`). `GET /v1/status` (OS/app/Tailscale:
  running, latest, update available, progress; restart needed), `POST /v1/check`,
  `/v1/os/install`, `/v1/app/update`, `/v1/tailscale/update`, `/v1/reboot`. The app
  replaces its apt/flag-file update code with these calls when `BOX_API_SOCKET` is
  set. Suggested app tweaks: use the API on the Software updates page, add
  `GET /healthz`.

## Release manifest

`manifest.json` (+ `manifest.json.sig`: Ed25519 over the file's bytes, key pinned
in the OS image) published as assets of the latest GitHub release of this repo:
`https://github.com/siesta5787/pi2zeroserver/releases/latest/download/manifest.json`.
Lists the OS bundle (URL, version; RAUC checks its own signature) and the app
tarball (URL, version, SHA-256). The agent checks it at boot and every ~12 h.

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
   - ✅ 5e: Tailscale updates from the agent: latest version from
     pkgs.tailscale.com, signing keys checked against pinned root keys,
     streamed download hashed and signature-checked, only the two binaries
     unpacked, then tailscale-activate. HTTPS via the system libcurl (the build
     fails if the agent doesn't link it) with ca-certificates. Verified against
     Tailscale's real key chain in tests; QEMU checks it runs and fails cleanly
     offline. Still to verify on the Pi with internet.
   - ✅ 5c: signed release manifest. Agent checks
     `releases/latest/download/manifest.json` (+ `.sig`, Ed25519, key pinned in
     /etc/box) a minute after boot and every ~12 h, shows "system update
     available" with an Install button. `tools/publish-release.sh` builds, signs
     and publishes it (pinning a kid-phone-server release by SHA-256).
   - ✅ 5f: app hosting. kid-phone-server runs as user `app` (fixed UID 950),
     sandboxed, from /data/app/current; first install happens by itself once the
     manifest names a release (SHA-256 checked, unpacked so nothing escapes);
     `app-activate` snapshots the DB, switches, health-checks and restores +
     switches back on failure. Box API on /run/box/api.sock (root:app 0660), see
     docs/box-api.md. Home LAN (port 80) gets /_system/ only; the app is on the
     tailnet front (127.0.0.1:8081) published with `tailscale serve --https=443`,
     forwarded with X-Forwarded-Proto: https. QEMU test runs the real
     kid-phone-server 0.18.6 binary.
6. Onboarding.
   - ✅ 6a: Wi-Fi page (scan, join, forget) through iwd's D-Bus API via `busctl`;
     passwords written as iwd network files on /data, restored if the join
     fails (and the box rejoins the network it was on). Setup hotspot with
     DHCP and captive portal (DNS + redirect), fallback hotspot when offline.
     The hotspot runs on hostapd: iwd's AP mode failed on the real Pi
     (brcmfmac rejects its START_AP).
     "Connect Tailscale" with sign-in link and QR code. QEMU test runs it on
     three mac80211_hwsim radios (box, home AP, phone).
     Still to verify on the Pi: hostapd hotspot, phones' captive portal
     prompts, the Tailscale sign-in with internet.
   - ✅ 6b: forgotten password and factory reset via a file on the boot
     partition, and an "Erase everything" button; QEMU test covers both.
     Later: a box name (several boxes on one network would all be box.local).
7. Hardening.
   - ✅ Firewall (box-firewall.service, /etc/box/firewall.nft, own nftables
     table): in only TCP 80, HTTPS on tailscale0, mDNS, DHCP/DHCPv6 replies, the
     hotspot's DHCP and captive DNS, Tailscale's UDP 41641, ICMP; no forwarding.
     QEMU checks it from a separate network namespace.
   - ✅ Night updates: between 2 and 5 am in the owner's time zone (start
     spread over two hours per box and night), check, install a newer
     Tailscale, then a newer system and restart. On by default; switch and
     time zone on the status page (the browser's zone preselected). App
     updates stay manual.
   - ✅ box_release_defconfig: root locked, no gettys, no curl command, no dev
     BusyBox applets. Release keys: tools/make-release-keys.sh puts them
     outside the repo and writes local/release.env (paths) for build.sh and
     publish-release.sh.
   - Still open: where the owner keeps the release keys, and moving devices
     from dev to release keys (see "Release keys").
8. Releases: CI pipeline, signed manifest, Imager repo, novice guide.
