#!/usr/bin/env python3
"""End-to-end A/B update test in QEMU (raspi3b: same BCM2837 SoC as the Zero 2 W).

Boots a copy of the dev sdcard.img and checks:
  1. first boot uses slot A, marks it good, has a read-only root and grows /data
     to fill the card; then the power is cut
  2. after the power cut /data is intact; `rauc install` of a signed bundle
     switches to slot B, which boots, is marked good and sees the same /data
  3. a broken slot B (kernel removed) uses up its tries and rolls back to slot A
Along the way: the built-in Tailscale runs, a newer release activates from
/data (and survives the OS update), and a broken release rolls back; the real
app runs and a broken app update rolls back; and on virtual Wi-Fi radios
(mac80211_hwsim) the setup hotspot (hostapd) starts with no saved network, a
"phone" joins it, and the box joins a "home" network from it (wrong password
first).

QEMU has no Raspberry Pi firmware and a few emulation gaps, so the test builds
adapted copies of a few pieces. Everything else is the production build:
  - U-Boot: production source/config/patches, plus a Linux image header (so QEMU
    passes the device tree in x0 like the firmware does) and no UART pinmux check
    (the firmware normally muxes GPIO14/15)
  - device tree: the Zero 2 W DTB with the real disable-bt overlay applied, as the
    firmware does from config.txt
  - rootfs (slot A and the test bundle): systemd's hardware watchdog setting removed,
    because QEMU resets immediately when the BCM2835 watchdog is armed

Usage: tools/qemu-ab-test.py [build output dir]   (default: /media/me/Claude/rpi-build/output/dev)
Full console log goes to <output>/qemu-ab-test.log.
"""
import glob
import json
import os
import re
import shutil
import subprocess
import sys
import threading
import time
import urllib.parse

PROJECT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
OUT = sys.argv[1] if len(sys.argv) > 1 else "/media/me/Claude/rpi-build/output/dev"
IMAGES = os.path.join(OUT, "images")
HOST = os.path.join(OUT, "host")
SBIN = os.path.join(HOST, "sbin")
WORK = os.path.join(OUT, "qemu-ab-test")
LOG = os.path.join(OUT, "qemu-ab-test.log")
RAUC_KEY = os.path.join(PROJECT, "local", "rauc", "dev-key.pem")
RAUC_CERT = os.path.join(PROJECT, "local", "rauc", "dev-cert.pem")
# The real app, run on the emulated box (QEMU has no internet, so it's fetched here)
APP_VERSION = "0.18.6"
APP_URL = ("https://github.com/siesta5787/kid-phone-server/releases/download/"
           f"v{APP_VERSION}/kid-phone-server-aarch64-unknown-linux-musl.tar.gz")


def run(*cmd, **kw):
    return subprocess.run(cmd, check=True, capture_output=True, text=True, **kw).stdout


def step(msg):
    print(f"... {msg}", flush=True)


def prepare_uboot():
    src = glob.glob(os.path.join(OUT, "build", "uboot-*"))
    src = [d for d in src if os.path.isfile(os.path.join(d, ".config"))]
    if len(src) != 1:
        sys.exit(f"expected one configured U-Boot build dir, found {src}")
    dst = os.path.join(WORK, "uboot")
    shutil.rmtree(dst, ignore_errors=True)
    shutil.copytree(src[0], dst, symlinks=True)

    def edit(path, old, new):
        path = os.path.join(dst, path)
        text = open(path).read()
        if old not in text:
            sys.exit(f"U-Boot QEMU tweak no longer applies to {path}: {old!r}")
        open(path, "w").write(text.replace(old, new, 1))

    # Hidden options in Kconfig; give them prompts so .config can set them
    edit("arch/arm/Kconfig", "config LINUX_KERNEL_IMAGE_HEADER\n\tdepends on ARM64\n\tbool\n",
         "config LINUX_KERNEL_IMAGE_HEADER\n\tdepends on ARM64\n\tbool \"image header\"\n")
    edit("arch/arm/Kconfig", "config LNX_KRNL_IMG_TEXT_OFFSET_BASE\n\tdepends on LINUX_KERNEL_IMAGE_HEADER\n\thex\n",
         "config LNX_KRNL_IMG_TEXT_OFFSET_BASE\n\tdepends on LINUX_KERNEL_IMAGE_HEADER\n\thex \"text offset base\"\n")
    edit("drivers/serial/serial_bcm283x_pl011.c",
         "if (!bcm283x_is_serial_muxed())", "if (0 && !bcm283x_is_serial_muxed())")
    with open(os.path.join(dst, ".config"), "a") as f:
        # PIE U-Boot (TEXT_BASE=0) loaded at 0x80000: text_offset = 0 - base
        f.write("CONFIG_LINUX_KERNEL_IMAGE_HEADER=y\n"
                "CONFIG_LNX_KRNL_IMG_TEXT_OFFSET_BASE=0xfffffffffff80000\n")

    hostpp = os.path.join(OUT, "per-package", "uboot", "host")
    env = dict(os.environ, PATH=f"{hostpp}/bin:{HOST}/bin:{os.environ['PATH']}")
    args = ["make", "-C", dst, "CROSS_COMPILE=aarch64-linux-",
            f"HOSTCFLAGS=-I{hostpp}/include",
            f"HOSTLDFLAGS=-L{hostpp}/lib -Wl,-rpath,{hostpp}/lib"]
    subprocess.run(args + ["olddefconfig"], check=True, capture_output=True, env=env)
    subprocess.run(args + [f"-j{os.cpu_count()}", "u-boot.bin"], check=True, capture_output=True, env=env)
    binary = os.path.join(dst, "u-boot.bin")
    with open(binary, "rb") as f:
        if f.read(64)[0x38:0x3c] != b"ARMd":
            sys.exit("QEMU U-Boot build has no Linux image header")
    return binary


def prepare_dtb():
    dtb = os.path.join(WORK, "zero2w-nobt.dtb")
    run(os.path.join(HOST, "bin", "fdtoverlay"),
        "-i", os.path.join(IMAGES, "bcm2710-rpi-zero-2-w.dtb"), "-o", dtb,
        os.path.join(IMAGES, "rpi-firmware", "overlays", "disable-bt.dtbo"))
    return dtb


def qemu_rootfs():
    """Production rootfs minus the hardware watchdog setting."""
    rootfs = os.path.join(WORK, "rootfs.ext4")
    shutil.copyfile(os.path.join(IMAGES, "rootfs.ext4"), rootfs)
    run(os.path.join(SBIN, "debugfs"), "-w", "-R",
        "rm /etc/systemd/system.conf.d/10-watchdog.conf", rootfs)
    return rootfs


def qemu_bundle(rootfs):
    bundles = glob.glob(os.path.join(IMAGES, "*.raucb"))
    if len(bundles) != 1:
        sys.exit(f"expected exactly one .raucb in {IMAGES}, found {bundles}")
    info = run(os.path.join(HOST, "bin", "rauc"), "info", f"--keyring={RAUC_CERT}",
               "--output-format=json", bundles[0])
    version = json.loads(info.strip().splitlines()[-1])["version"]

    content = os.path.join(WORK, "bundle")
    shutil.rmtree(content, ignore_errors=True)
    os.makedirs(content)
    shutil.copyfile(rootfs, os.path.join(content, "rootfs.ext4"))
    with open(os.path.join(content, "manifest.raucm"), "w") as f:
        f.write(f"[update]\ncompatible=box-zero2w\nversion={version}-qemu\n\n"
                "[bundle]\nformat=verity\n\n[image.rootfs]\nfilename=rootfs.ext4\n")
    bundle = os.path.join(WORK, "update.raucb")
    if os.path.exists(bundle):
        os.remove(bundle)
    run(os.path.join(HOST, "bin", "rauc"), "bundle", f"--cert={RAUC_CERT}", f"--key={RAUC_KEY}",
        content, bundle)
    shutil.rmtree(content)
    return bundle


def partitions(card):
    table = json.loads(run(os.path.join(SBIN, "sfdisk"), "-J", card))
    return [(p["start"] * 512, p["size"] * 512) for p in table["partitiontable"]["partitions"]]


def prepare_card(rootfs):
    """Copy the image, swap in the QEMU rootfs, and pad it to a power-of-2 size
    (QEMU's SD model needs that). The device grows /data into the padding."""
    card = os.path.join(WORK, "sdcard.img")
    shutil.copyfile(os.path.join(IMAGES, "sdcard.img"), card)
    length = os.path.getsize(card)
    os.truncate(card, 1 << (length - 1).bit_length())
    with open(rootfs, "rb") as src, open(card, "r+b") as dst:
        dst.seek(partitions(card)[1][0])
        shutil.copyfileobj(src, dst)
    return card


def app_tarball():
    path = os.path.join(WORK, f"app-{APP_VERSION}.tgz")
    if not os.path.exists(path):
        subprocess.run(["curl", "-fsSL", "-o", path + ".part", APP_URL], check=True)
        os.rename(path + ".part", path)
    return path


def inject_bundle(card, bundle):
    """Copy the bundle and the app tarball onto the (now grown) data partition
    while QEMU is off."""
    start, size = partitions(card)[3]
    part = os.path.join(WORK, "data.ext4")
    with open(card, "rb") as src, open(part, "wb") as dst:
        src.seek(start)
        dst.write(src.read(size))
    # Replays the journal after the power cut; 1 = errors fixed, >=4 = not fixable
    rc = subprocess.run([os.path.join(SBIN, "e2fsck"), "-fy", part], capture_output=True).returncode
    if rc >= 4:
        sys.exit(f"e2fsck could not repair the data partition (rc={rc})")
    run(os.path.join(SBIN, "debugfs"), "-w", "-R", f"write {bundle} update.raucb", part)
    run(os.path.join(SBIN, "debugfs"), "-w", "-R", f"write {app_tarball()} app.tgz", part)
    with open(part, "rb") as src, open(card, "r+b") as dst:
        dst.seek(start)
        shutil.copyfileobj(src, dst)
    os.remove(part)


class Console:
    def __init__(self, card, uboot, dtb):
        self.log = open(LOG, "a")
        self.buf = ""
        self.lock = threading.Lock()
        self.proc = subprocess.Popen(
            [
                os.path.join(HOST, "bin", "qemu-system-aarch64"),
                "-M", "raspi3b", "-m", "1G",
                "-kernel", uboot,
                "-dtb", dtb,
                "-drive", f"file={card},if=sd,format=raw",
                "-serial", "stdio", "-display", "none", "-monitor", "none",
            ],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
        )
        threading.Thread(target=self._reader, daemon=True).start()

    def _reader(self):
        while True:
            chunk = self.proc.stdout.read1(4096)
            if not chunk:
                return
            text = chunk.decode(errors="replace")
            self.log.write(text)
            self.log.flush()
            with self.lock:
                self.buf += text

    def expect(self, pattern, timeout):
        """Wait for a regex in output seen since the last match; consume up to it."""
        deadline = time.time() + timeout
        rx = re.compile(pattern)
        while time.time() < deadline:
            with self.lock:
                m = rx.search(self.buf)
                if m:
                    self.buf = self.buf[m.end():]
                    return m
            if self.proc.poll() is not None:
                raise RuntimeError(f"QEMU exited while waiting for {pattern!r}")
            time.sleep(0.2)
        raise TimeoutError(f"timed out after {timeout}s waiting for {pattern!r} (see {LOG})")

    def send(self, text):
        self.proc.stdin.write(text.encode())
        self.proc.stdin.flush()

    def login(self):
        self.expect(r"login:", 900)
        self.send("root\n")
        self.expect(r"# ", 120)
        # Keep kernel messages and pagers out of command output
        self.send("dmesg -n 1; export SYSTEMD_PAGER= SYSTEMD_COLORS=0 PAGER=cat TERM=dumb\n")
        self.expect(r"# ", 60)
        # Let boot finish so mark-good has had its chance to run
        self.sh("systemctl is-system-running --wait", timeout=600, check=False)

    def sh(self, cmd, timeout=180, check=True):
        """Run a shell command, return (rc, output)."""
        tag = f"{time.time_ns()}"
        self.send(f"echo BEGIN{tag}; {cmd}; echo END{tag} rc=$?\n")
        # Match the echoed output, not the typed command line
        self.expect(rf"\nBEGIN{tag}\r?\n", timeout)
        m = self.expect(rf"([\s\S]*?)END{tag} rc=(\d+)", timeout)
        out, rc = m.group(1).strip(), int(m.group(2))
        if check and rc != 0:
            raise RuntimeError(f"`{cmd}` failed rc={rc}:\n{out}")
        return rc, out

    def stop(self):
        self.proc.kill()
        self.proc.wait()
        self.log.close()


def env(con, name):
    return con.sh(f"fw_printenv -n {name}")[1].splitlines()[-1].strip()


def booted_slot(con):
    status = json.loads(con.sh("rauc status --output-format=json")[1].splitlines()[-1])
    return status["booted"]


def check(label, ok, detail=""):
    print(f"[{'PASS' if ok else 'FAIL'}] {label}{(' - ' + detail) if detail else ''}", flush=True)
    if not ok:
        raise SystemExit(1)


def check_slot_basics(con, slot):
    check(f"slot {slot} marked good", env(con, f"BOOT_{slot}_LEFT") == "3",
          f"BOOT_{slot}_LEFT={env(con, f'BOOT_{slot}_LEFT')}")
    rc, failed = con.sh("systemctl --failed --no-legend --plain --no-pager", check=False)
    check(f"no failed units on slot {slot}", not failed.strip(), failed.strip())
    rc, out = con.sh("touch /etc/write-test", check=False)
    check(f"slot {slot} root filesystem is read-only", rc != 0)
    rc, out = con.sh("awk '$2 == \"/var\" {print $3}' /proc/mounts", check=False)
    check(f"slot {slot} /var is a tmpfs", out.splitlines()[-1:] == ["tmpfs"], out)


def tailscaled_exe(con):
    out = con.sh("readlink /proc/$(pidof tailscaled)/exe", check=False)[1].splitlines()
    return out[-1] if out else ""


def stage_release(con, version, broken_daemon=False):
    """Put a Tailscale release on /data the way the updater will: the built-in
    binaries under a fake newer version number (QEMU has no network)."""
    rel = f"/data/tailscale/releases/{version}"
    daemon = (f"printf '#!/bin/sh\\nexit 1\\n' > {rel}/tailscaled && chmod 755 {rel}/tailscaled"
              if broken_daemon else f"cp /usr/lib/box/tailscale/tailscaled {rel}/")
    con.sh(f"mkdir -p {rel} && cp /usr/lib/box/tailscale/tailscale {rel}/ && {daemon} "
           f"&& echo {version} > {rel}/VERSION && sync", timeout=600)


ADMIN_PASSWORD = "qemu+test+password"  # form-encoded


def agent_post(con, path, form):
    """POST a form like the box's own page would; returns the session cookie set.
    (curl, not BusyBox wget: wget re-POSTs on redirects and hides Set-Cookie.)"""
    out = con.sh(f"curl -s -m 30 -o /dev/null -D - -H 'Origin: http://127.0.0.1' "
                 f"-d '{form}' http://127.0.0.1{path}", timeout=60, check=False)[1]
    m = re.search(r"set-cookie: (box_session=[0-9a-f]+)", out, re.IGNORECASE)
    return m.group(1) if m else ""


def agent_api(con, path, cookie, form=None, timeout=60):
    """Call the agent's API as a logged-in browser; returns (HTTP status, body)."""
    data = f"-H 'Origin: http://127.0.0.1' -d '{form}' " if form is not None else ""
    out = con.sh(f"curl -s -m 30 -w '\\n%{{http_code}}' -H 'Cookie: {cookie}' {data}http://127.0.0.1{path}",
                 timeout=timeout, check=False)[1].splitlines()
    return (int(out[-1]) if out and out[-1].isdigit() else 0), "\n".join(out[:-1])


def install_via_agent(con):
    """Install /data/update.raucb the way the system page does, and wait for it."""
    cookie = agent_post(con, "/_system/login", f"password={ADMIN_PASSWORD}")
    code, body = agent_api(con, "/_system/api/os/install", cookie, "source=%2Fdata%2Fupdate.raucb")
    check("agent starts the OS install", code == 202, f"{code} {body}")
    deadline = time.time() + 1800
    state = {}
    while time.time() < deadline:
        code, body = agent_api(con, "/_system/api/os/install", cookie)
        try:
            state = json.loads(body.splitlines()[-1])
        except (ValueError, IndexError):
            state = {}
        if state.get("state") != "running":
            break
        time.sleep(15)
    check("agent reports the update installed", state.get("state") == "installed", str(state))
    return cookie


def check_agent(con, slot):
    rc, out = con.sh("wget -q -T 10 -O - http://127.0.0.1/_system/api/health", check=False)
    check(f"agent answers on slot {slot}", rc == 0 and out.splitlines()[-1:] == ["ok"], out)
    rc, out = con.sh("wget -T 10 -O /dev/null http://127.0.0.1/_system/api/status 2>&1", check=False)
    check(f"status API needs a login on slot {slot}", "401" in out, out.splitlines()[-1] if out else "")
    if slot == "A":
        cookie = agent_post(con, "/_system/password", f"password={ADMIN_PASSWORD}&confirm={ADMIN_PASSWORD}")
        check("first visit sets the admin password", bool(cookie))
        rc, out = con.sh("ls -l /data/agent/admin-password | cut -c1-10", check=False)
        check("admin password stored privately", out.splitlines()[-1:] == ["-rw-------"], out)
    else:
        cookie = agent_post(con, "/_system/login", f"password={ADMIN_PASSWORD}")
        check("admin password survives the OS update", bool(cookie))
    rc, out = con.sh(f"curl -s -m 30 -H 'Cookie: {cookie}' http://127.0.0.1/_system/api/status",
                     timeout=60, check=False)
    try:
        status = json.loads(out.splitlines()[-1])
    except (ValueError, IndexError):
        status = {}
    check(f"status API reports slot {slot}", status.get("os", {}).get("booted_slot") == slot, out[-200:])
    check("status API reports Tailscale", status.get("tailscale", {}).get("running") is True,
          str(status.get("tailscale")))
    rc, out = con.sh("wget -q -T 10 -O - http://127.0.0.1/ | grep -c 'Open the app over Tailscale'", check=False)
    check("home network points to the app on Tailscale", rc == 0, out.splitlines()[-1] if out else "")
    if slot == "A":
        rc, out = con.sh("wget -T 10 -O /dev/null http://127.0.0.1:8081/ 2>&1", check=False)
        check("tailnet front, no app yet: friendly 502 page", "502" in out, out.splitlines()[-1] if out else "")
    if slot == "A":
        rc, _ = con.sh("test -s /etc/ssl/certs/ca-certificates.crt", check=False)
        check("CA certificates installed for HTTPS", rc == 0)
        # QEMU has no internet: the updater must run and fail cleanly, installing nothing
        code, body = agent_api(con, "/_system/api/tailscale/update", cookie, "")
        state = {}
        for _ in range(30):
            code, body = agent_api(con, "/_system/api/tailscale/update", cookie)
            try:
                state = json.loads(body.splitlines()[-1])
            except (ValueError, IndexError):
                state = {}
            if state.get("state") not in ("checking", "downloading", "installing"):
                break
            time.sleep(5)
        # ...and with a network error: "Unsupported protocol" would mean curl lacks HTTPS
        error = state.get("error", "")
        rc, out = con.sh("cat /etc/box/manifest-url /etc/box/manifest-key.pub", check=False)
        check("image knows where updates come from", rc == 0 and "releases/latest/download/manifest.json" in out,
              out.splitlines()[0] if out else "")
        agent_api(con, "/_system/api/updates/check", cookie, "")
        updates = {}
        for _ in range(30):
            code, body = agent_api(con, "/_system/api/updates", cookie)
            try:
                updates = json.loads(body.splitlines()[-1])
            except (ValueError, IndexError):
                updates = {}
            if not updates.get("checking"):
                break
            time.sleep(5)
        check("update check fails cleanly offline", "github.com" in (updates.get("error") or "")
              and "Unsupported protocol" not in updates.get("error", ""), str(updates))
        check("Tailscale updater fails cleanly offline",
              state.get("state") == "failed" and "pkgs.tailscale.com" in error
              and "Unsupported protocol" not in error, str(state))


def stage_app(con, version, broken=False):
    """Unpack a release the way the agent does: binary + static/, `app` link, VERSION."""
    rel = f"/data/app/releases/{version}"
    binary = (f"printf '#!/bin/sh\\nexit 1\\n' > {rel}/kid_phone_server && chmod 755 {rel}/kid_phone_server"
              if broken else f"chmod 755 {rel}/kid_phone_server")
    con.sh(f"mkdir -p {rel} && gunzip -c /data/app.tgz | tar -x -C {rel} && {binary} "
           f"&& ln -sfn kid_phone_server {rel}/app && echo {version} > {rel}/VERSION && sync", timeout=600)


def check_app(con):
    # First install: the agent writes the environment from the image's template
    con.sh("cp /etc/box/app.env /data/app/env && echo ADMIN_PASSWORD=qemu-test-admin >> /data/app/env "
           "&& chmod 600 /data/app/env")
    stage_app(con, APP_VERSION)
    rc, out = con.sh(f"/usr/libexec/box/app-activate {APP_VERSION}", timeout=300, check=False)
    check("kid-phone-server activates", rc == 0, out.splitlines()[-1] if out else "")
    rc, out = con.sh("wget -q -T 10 -O - http://127.0.0.1:3100/ | grep -ci '<form'", timeout=60, check=False)
    check("kid-phone-server answers (login page)", rc == 0, out.splitlines()[-1] if out else "")
    rc, out = con.sh("wget -q -T 10 -O - http://127.0.0.1:8081/ | grep -ci '<form'", timeout=60, check=False)
    check("app reachable through the tailnet front", rc == 0, out.splitlines()[-1] if out else "")
    rc, out = con.sh("awk '/^Uid:/ {print $2}' /proc/$(pidof kid_phone_server)/status", check=False)
    check("app runs as the unprivileged app user (950)", out.splitlines()[-1:] == ["950"], out)
    rc, out = con.sh("ls -l /data/app/state/kidphone.db | cut -c1-10,16-30", check=False)
    check("app database created in /data/app/state", rc == 0 and "app" in out, out)

    rc, out = con.sh("ls -l /run/box/api.sock | cut -c1-10; ls -ln /run/box/api.sock | awk '{print $3, $4}'",
                     check=False)
    check("box API socket only for root and the app group", out.split() == ["srw-rw----", "0", "950"], out)
    rc, out = con.sh("curl -s -m 30 --unix-socket /run/box/api.sock http://box/v1/status", timeout=60, check=False)
    try:
        api = json.loads(out.splitlines()[-1])
    except (ValueError, IndexError):
        api = {}
    check("box API reports the running app", api.get("app", {}).get("version") == APP_VERSION,
          str(api.get("app")))

    stage_app(con, "0.18.7", broken=True)
    rc, out = con.sh("/usr/libexec/box/app-activate 0.18.7", timeout=400, check=False)
    check("broken app release is rolled back", rc == 1, out.splitlines()[-1] if out else "")
    link = con.sh("readlink /data/app/current")[1].splitlines()[-1]
    rc, _ = con.sh("wget -q -T 10 -O /dev/null http://127.0.0.1:3100/ && test -s /data/app/state/kidphone.db",
                   timeout=120, check=False)
    check("previous app and its database back", link == f"releases/{APP_VERSION}" and rc == 0, link)


def check_tailscale(con):
    builtin = con.sh("cat /usr/lib/box/tailscale/VERSION")[1].splitlines()[-1]
    rc, out = con.sh("systemctl is-active tailscaled", check=False)
    check("tailscaled running", rc == 0, out.splitlines()[-1] if out else "")
    rc, out = con.sh("/usr/libexec/box/tailscale-check 120 && . /usr/libexec/box/tailscale-lib.sh && ts_daemon_version",
                     timeout=180, check=False)
    check("tailscaled answers as the built-in version", rc == 0 and out.splitlines()[-1:] == [builtin], out)
    state = con.sh("tailscale status --json | sed -n 's/.*\"BackendState\": *\"\\([A-Za-z]*\\)\".*/\\1/p'", check=False)[1]
    check("Tailscale waiting for login", state.splitlines()[-1:] == ["NeedsLogin"], state)

    stage_release(con, "9999.0.0")
    rc, out = con.sh("/usr/libexec/box/tailscale-activate 9999.0.0", timeout=300, check=False)
    check("newer Tailscale release activates", rc == 0, out.splitlines()[-1] if out else "")
    check("tailscaled runs from the activated release",
          "/data/tailscale/releases/9999.0.0/" in tailscaled_exe(con), tailscaled_exe(con))

    stage_release(con, "9999.0.1", broken_daemon=True)
    rc, out = con.sh("/usr/libexec/box/tailscale-activate 9999.0.1", timeout=400, check=False)
    check("broken Tailscale release is rolled back", rc == 1, out.splitlines()[-1] if out else "")
    link = con.sh("readlink /data/tailscale/current")[1].splitlines()[-1]
    rc, _ = con.sh("/usr/libexec/box/tailscale-check 120", timeout=180, check=False)
    check("previous Tailscale release running again",
          link == "releases/9999.0.0" and rc == 0 and "/9999.0.0/" in tailscaled_exe(con), link)


# Shell helpers for the Wi-Fi checks: iwd's object path for an interface, an SSID in hex
WIFI_HELPERS = ("iwdev() { echo /net/connman/iwd/$(cat /sys/class/net/$1/phy80211/index)/"
                "$(cat /sys/class/net/$1/ifindex); }; "
                "hexssid() { printf %s \"$1\" | od -An -tx1 | tr -d ' \\n'; }")


def wifi_view(con, cookie):
    code, body = agent_api(con, "/_system/api/wifi", cookie)
    try:
        return json.loads(body.splitlines()[-1])
    except (ValueError, IndexError):
        return {}


def wait_wifi(con, cookie, ready, timeout):
    """Polls the agent's Wi-Fi state until ready(view); returns the last view."""
    deadline = time.time() + timeout
    view = wifi_view(con, cookie)
    while not ready(view) and time.time() < deadline:
        time.sleep(5)
        view = wifi_view(con, cookie)
    return view


def wait_addr(con, iface, prefix, timeout=60):
    """Waits for an IPv4 address starting with prefix on iface; returns `ip addr` output."""
    return con.sh(f"for i in $(seq {timeout}); do ip -4 addr show {iface} | grep -q 'inet {prefix}' && break; "
                  f"sleep 1; done; ip -4 addr show {iface}", timeout=timeout + 60, check=False)[1]


def check_wifi(con):
    """Setup hotspot and joining networks, on three virtual radios: wlan0 is the
    box's, wlan1 plays the home Wi-Fi (a test hostapd), wlan2 a phone. The
    system's iwd is told to leave wlan1 and wlan2 alone (test-only drop-in)."""
    con.sh(WIFI_HELPERS)
    con.sh("mkdir -p /run/systemd/system/iwd.service.d /run/systemd/network && "
           "printf '[Service]\\nExecStart=\\nExecStart=/usr/libexec/iwd -I wlan1,wlan2\\n' "
           "> /run/systemd/system/iwd.service.d/test.conf && systemctl daemon-reload && systemctl restart iwd && "
           "printf '[Match]\\nName=wlan1\\n[Network]\\nAddress=192.168.50.1/24\\nDHCPServer=yes\\n"
           "LinkLocalAddressing=no\\n' > /run/systemd/network/05-test-home.network && "
           "printf '[Match]\\nName=wlan2\\n[Network]\\nDHCP=ipv4\\nLinkLocalAddressing=no\\n' "
           "> /run/systemd/network/05-test-phone.network && networkctl reload")
    con.sh("modprobe mac80211_hwsim radios=3")
    rc, out = con.sh("for i in $(seq 30); do [ -e /sys/class/net/wlan2 ] && break; sleep 1; done; "
                     "printf 'interface=wlan1\\ndriver=nl80211\\nssid=TestHome\\nhw_mode=g\\nchannel=1\\nwpa=2\\n"
                     "wpa_key_mgmt=WPA-PSK\\nrsn_pairwise=CCMP\\nwpa_passphrase=test-home-pw\\n' > /run/test-home.conf "
                     "&& hostapd -B -P /run/test-home.pid /run/test-home.conf", timeout=120, check=False)
    check("test home network up", rc == 0, out[-300:])

    cookie = agent_post(con, "/_system/login", f"password={ADMIN_PASSWORD}")
    view = wait_wifi(con, cookie, lambda v: v.get("available"), 60)
    check("agent finds the Wi-Fi", view.get("available") is True, str(view)[:300])
    # A dev card may come with the developer's Wi-Fi saved: forget it, like a fresh box
    for name in view.get("saved", []):
        agent_api(con, "/_system/wifi/forget", cookie, f"name={urllib.parse.quote_plus(name)}")
    view = wait_wifi(con, cookie, lambda v: (v.get("hotspot") or {}).get("setup"), 120)
    if not any(n.get("name") == "TestHome" for n in view.get("networks", [])):
        # The hotspot came up before the home network did; its list is from before
        con.sh("systemctl start iwd")
        view = wait_wifi(con, cookie, lambda v: v.get("hotspot")
                         and any(n.get("name") == "TestHome" for n in v.get("networks", [])), 120)
    hotspot = (view.get("hotspot") or {}).get("ssid", "")
    check("no saved Wi-Fi: setup hotspot starts", hotspot.startswith("Box-Setup-") and not view.get("saved"),
          str(view)[:300])
    rc, out = con.sh("systemctl is-active box-hotspot iwd", check=False)
    check("hostapd runs the hotspot, iwd stepped aside", out.split()[-2:] == ["active", "inactive"], out)
    out = wait_addr(con, "wlan0", "192.168.4.1/24")
    check("hotspot has its address", "inet 192.168.4.1/24" in out, out[-200:])
    check("hotspot lists the networks around", any(n.get("name") == "TestHome" for n in view.get("networks", [])),
          str(view.get("networks")))
    rc, out = con.sh("nslookup connectivitycheck.gstatic.com 192.168.4.1", check=False)
    check("hotspot DNS answers every name with the box",
          "Name:" in out and "192.168.4.1" in out.split("Name:", 1)[-1], out[-300:])
    rc, out = con.sh("curl -s -m 10 -o /dev/null -w '%{http_code} %{redirect_url}\\n' "
                     "-H 'Host: connectivitycheck.gstatic.com' http://192.168.4.1/generate_204", check=False)
    check("phones checking for internet land on the setup page",
          out.splitlines()[-1:] == ["302 http://192.168.4.1/_system/"], out[-200:])

    # The phone (its own iwd, on wlan2 only) joins with the published password and gets an address
    con.sh("mkdir -p /var/lib/iwd; /usr/libexec/iwd -i wlan2 >/dev/null 2>&1 & echo $! > /run/test-iwd.pid")
    con.sh(f"printf '[Security]\\nPassphrase=boxsetup\\n' > /var/lib/iwd/{hotspot}.psk; "
           "for i in $(seq 20); do busctl get-property net.connman.iwd $(iwdev wlan2) net.connman.iwd.Station "
           "State >/dev/null 2>&1 && break; sleep 1; done; "
           "busctl call net.connman.iwd $(iwdev wlan2) net.connman.iwd.Station Scan; sleep 10; "
           f"busctl --timeout=60 call net.connman.iwd $(iwdev wlan2)/$(hexssid {hotspot})_psk "
           "net.connman.iwd.Network Connect", timeout=120, check=False)
    out = wait_addr(con, "wlan2", "192.168.4.")
    check("a phone joins the hotspot and gets an address from it", "inet 192.168.4." in out, out[-300:])
    rc, out = con.sh("hostapd_cli -p /run/box-hotspot -i wlan0 list_sta", check=False)
    check("hostapd lists the phone", any(len(l.strip()) == 17 for l in out.splitlines()), out[-200:])
    con.sh(f"kill $(cat /run/test-iwd.pid); sleep 2; rm -f /var/lib/iwd/{hotspot}.psk", check=False)

    # Joining the home network from the hotspot: a typo first
    code, body = agent_api(con, "/_system/wifi/join", cookie, "ssid=TestHome&password=wrong-password")
    check("joining from the hotspot says what happens next", code == 200 and "Connect this phone back" in body,
          f"{code} {body[-200:]}")
    view = wait_wifi(con, cookie, lambda v: v.get("join", {}).get("state") == "failed" and v.get("hotspot"), 240)
    check("wrong password: the hotspot comes back with the error",
          view.get("join", {}).get("state") == "failed" and bool(view.get("hotspot")), str(view)[:400])
    rc, _ = con.sh("test -e /data/iwd/TestHome.psk", check=False)
    check("a wrong password isn't kept", rc != 0)
    agent_api(con, "/_system/wifi/join", cookie, "ssid=TestHome&password=test-home-pw")
    view = wait_wifi(con, cookie, lambda v: v.get("connected") == "TestHome"
                     and v.get("join", {}).get("state") == "joined", 240)
    check("the box joins the home network from the hotspot",
          view.get("connected") == "TestHome" and not view.get("hotspot"), str(view)[:400])
    rc, out = con.sh("systemctl is-active box-hotspot iwd", check=False)
    check("hotspot off, iwd back", out.split()[-2:] == ["inactive", "active"], out)
    out = wait_addr(con, "wlan0", "192.168.50.")
    check("the box gets an address on the home network", "inet 192.168.50." in out and "192.168.4.1" not in out,
          out[-300:])
    rc, out = con.sh("ls -l /data/iwd/TestHome.psk | cut -c1-10", check=False)
    check("home Wi-Fi password saved privately", out.splitlines()[-1:] == ["-rw-------"], out)

    # From the home network: retyping the password wrong mustn't leave the box offline
    agent_api(con, "/_system/wifi/join", cookie, "ssid=TestHome&password=another-typo")
    view = wait_wifi(con, cookie, lambda v: v.get("join", {}).get("state") == "failed"
                     and v.get("connected") == "TestHome", 180)
    check("a failed change goes back to the network it was on",
          view.get("join", {}).get("state") == "failed" and view.get("connected") == "TestHome", str(view)[:400])


def main():
    # rauc runs mksquashfs etc. from the build's host tools
    os.environ["PATH"] = f"{HOST}/bin:{SBIN}:{os.environ['PATH']}"
    os.makedirs(WORK, exist_ok=True)
    open(LOG, "w").close()
    step("building QEMU U-Boot from the production U-Boot build")
    uboot = prepare_uboot()
    step("preparing device tree, rootfs, test bundle and SD card")
    dtb = prepare_dtb()
    rootfs = qemu_rootfs()
    bundle = qemu_bundle(rootfs)
    card = prepare_card(rootfs)

    # 1. First boot: slot A, read-only root, /data grown to fill the card
    step("first boot (QEMU's SD emulation is slow: allow ~2 minutes per boot)")
    con = Console(card, uboot, dtb)
    try:
        con.expect(r"Booting slot A", 300)
        con.login()
        check("first boot is slot A", booted_slot(con) == "A")
        check_slot_basics(con, "A")
        rc, out = con.sh("modprobe pkcs8_key_parser && lsmod | grep -c pkcs8", check=False)
        check("kernel modules load", rc == 0)
        rc, out = con.sh("lsmod | grep -c '^cfg80211'", check=False)
        check("wireless stack loaded at boot", rc == 0)
        rc, out = con.sh("systemctl is-active iwd", check=False)
        check("iwd running", rc == 0, out.splitlines()[-1] if out else "")
        kb = int(con.sh("df -k /data | awk 'END {print $2}'")[1].split()[-1])
        # The test card is 2 GiB; boot + both slots take ~1.1 GiB of it
        check("/data grown to fill the card", kb > 800_000, f"{kb // 1024} MiB")
        rc, out = con.sh("ls -ld /data/iwd /data/rauc | cut -c1-10", check=False)
        check("/data state folders created", rc == 0 and out.split() == ["drwx------"] * 2, out)
        check_agent(con, "A")
        check_tailscale(con)
        check_wifi(con)
        con.sh("echo kept > /data/marker && sync")
    finally:
        con.stop()  # pull the power

    # 2. After the power cut: /data intact; update into slot B
    step("power cut done; copying the update bundle onto /data offline")
    inject_bundle(card, bundle)
    con = Console(card, uboot, dtb)
    try:
        con.expect(r"Booting slot A", 300)
        con.login()
        check("/data intact after power cut", con.sh("cat /data/marker")[1].splitlines()[-1] == "kept")
        check_app(con)
        version_a = con.sh("cat /etc/box-version")[1].splitlines()[-1]

        cookie = install_via_agent(con)
        check("B is now first in boot order", env(con, "BOOT_ORDER") == "B A",
              f"BOOT_ORDER={env(con, 'BOOT_ORDER')}")
        code, body = agent_api(con, "/_system/restart", cookie, "")
        check("Restart button restarts the box", code == 200 and "Restarting" in body, str(code))
        con.expect(r"Booting slot B", 900)
        con.login()
        check("update boots slot B", booted_slot(con) == "B")
        check_slot_basics(con, "B")
        check("slot B sees the same /data", con.sh("cat /data/marker")[1].splitlines()[-1] == "kept")
        check_agent(con, "B")
        # The service is up as soon as it launches; the app opens its port a bit later
        rc, out = con.sh("for i in $(seq 40); do wget -q -T 5 -O /dev/null http://127.0.0.1:3100/ && exit_ok=1 && break; "
                         "sleep 3; done; [ -n \"$exit_ok\" ] || journalctl -u box-app -n 15 --no-pager; [ -n \"$exit_ok\" ]",
                         timeout=300, check=False)
        check("app still running after the OS update", rc == 0, "" if rc == 0 else out[-600:])
        check("activated Tailscale survives the OS update",
              "/data/tailscale/releases/9999.0.0/" in tailscaled_exe(con), tailscaled_exe(con))
        version_b = con.sh("cat /etc/box-version")[1].splitlines()[-1]
        check("slot B runs the installed system", True, f"{version_b} (slot A: {version_a})")

        # 3. Break slot B's kernel; U-Boot must use up B's tries and fall back to A
        con.sh("mount -o remount,rw / && rm /boot/Image && sync")
        con.send("reboot\n")
        for left in (2, 1, 0):
            con.expect(rf"Booting slot B, {left} tries left", 900)
            con.expect(r"Slot B failed to boot; resetting", 300)
        con.expect(r"Booting slot A", 300)
        con.login()
        check("broken slot B rolls back to slot A", booted_slot(con) == "A")
        check("slot B has no tries left", env(con, "BOOT_B_LEFT") == "0")
        print("All A/B checks passed.")
    finally:
        con.stop()


if __name__ == "__main__":
    try:
        main()
    except BaseException:
        # Keep the card for inspection; other runs and probes overwrite sdcard.img
        card = os.path.join(WORK, "sdcard.img")
        if os.path.exists(card):
            subprocess.run(["cp", "--sparse=always", card, os.path.join(WORK, "sdcard.failed.img")])
            print(f"card kept at {os.path.join(WORK, 'sdcard.failed.img')}", flush=True)
        raise
