#!/usr/bin/env python3
"""End-to-end A/B update test in QEMU (raspi3b: same BCM2837 SoC as the Zero 2 W).

Boots a copy of the dev sdcard.img and checks:
  1. first boot uses slot A, marks it good, has a read-only root and grows /data
     to fill the card; then the power is cut
  2. after the power cut /data is intact; `rauc install` of a signed bundle
     switches to slot B, which boots, is marked good and sees the same /data
  3. a broken slot B (kernel removed) uses up its tries and rolls back to slot A
Along the way: the built-in Tailscale runs, a newer release activates from
/data (and survives the OS update), and a broken release rolls back.

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

PROJECT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
OUT = sys.argv[1] if len(sys.argv) > 1 else "/media/me/Claude/rpi-build/output/dev"
IMAGES = os.path.join(OUT, "images")
HOST = os.path.join(OUT, "host")
SBIN = os.path.join(HOST, "sbin")
WORK = os.path.join(OUT, "qemu-ab-test")
LOG = os.path.join(OUT, "qemu-ab-test.log")
RAUC_KEY = os.path.join(PROJECT, "local", "rauc", "dev-key.pem")
RAUC_CERT = os.path.join(PROJECT, "local", "rauc", "dev-cert.pem")


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


def inject_bundle(card, bundle):
    """Copy the bundle onto the (now grown) data partition while QEMU is off."""
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
    rc, out = con.sh("wget -T 10 -O /dev/null http://127.0.0.1/ 2>&1", check=False)
    check("no app installed: friendly 502 page", "502" in out, out.splitlines()[-1] if out else "")
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
