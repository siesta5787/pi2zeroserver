#!/usr/bin/env python3
"""End-to-end A/B update test in QEMU (raspi3b: same BCM2837 SoC as the Zero 2 W).

Boots a copy of the dev sdcard.img and checks:
  1. first boot uses slot A and marks it good
  2. `rauc install` of a signed bundle switches to slot B, which boots and is marked good
  3. a broken slot B (kernel removed) uses up its tries and rolls back to slot A

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


def prepare_card(rootfs, bundle):
    """Copy the image, swap in the QEMU rootfs, grow the data partition to fill a
    power-of-2 card (QEMU's SD model needs that size) and put the bundle on it."""
    card = os.path.join(WORK, "sdcard.img")
    shutil.copyfile(os.path.join(IMAGES, "sdcard.img"), card)

    length = os.path.getsize(card)
    os.truncate(card, 1 << (length - 1).bit_length())
    subprocess.run([os.path.join(SBIN, "sfdisk"), "--no-reread", "--no-tell-kernel", "-N", "4", card],
                   input=", +\n", check=True, capture_output=True, text=True)
    parts = partitions(card)

    with open(rootfs, "rb") as src, open(card, "r+b") as dst:
        dst.seek(parts[1][0])
        shutil.copyfileobj(src, dst)

    start, size = parts[3]
    part = os.path.join(WORK, "data.ext4")
    with open(card, "rb") as src, open(part, "wb") as dst:
        src.seek(start)
        dst.write(src.read(size))
    os.truncate(part, size)
    run(os.path.join(SBIN, "e2fsck"), "-fy", part)
    run(os.path.join(SBIN, "resize2fs"), part)
    run(os.path.join(SBIN, "debugfs"), "-w", "-R", f"write {bundle} update.raucb", part)
    with open(part, "rb") as src, open(card, "r+b") as dst:
        dst.seek(start)
        shutil.copyfileobj(src, dst)
    os.remove(part)
    return card


class Console:
    def __init__(self, card, uboot, dtb):
        self.log = open(LOG, "w")
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


def main():
    # rauc runs mksquashfs etc. from the build's host tools
    os.environ["PATH"] = f"{HOST}/bin:{SBIN}:{os.environ['PATH']}"
    os.makedirs(WORK, exist_ok=True)
    step("building QEMU U-Boot from the production U-Boot build")
    uboot = prepare_uboot()
    step("preparing device tree, rootfs, test bundle and SD card")
    dtb = prepare_dtb()
    rootfs = qemu_rootfs()
    card = prepare_card(rootfs, qemu_bundle(rootfs))
    step("booting (QEMU's SD emulation is slow: allow ~2 minutes per boot)")
    con = Console(card, uboot, dtb)
    try:
        # 1. First boot: slot A, marked good
        con.expect(r"Booting slot A", 300)
        con.login()
        check("first boot is slot A", booted_slot(con) == "A")
        check("slot A marked good", env(con, "BOOT_A_LEFT") == "3",
              f"BOOT_A_LEFT={env(con, 'BOOT_A_LEFT')}")
        rc, failed = con.sh("systemctl --failed --no-legend --plain --no-pager", check=False)
        check("no failed units on slot A", not failed.strip(), failed.strip())
        rc, out = con.sh("modprobe pkcs8_key_parser && lsmod | grep -c pkcs8", check=False)
        check("kernel modules load", rc == 0, out.splitlines()[-1] if out else "")
        rc, out = con.sh("lsmod | grep -c '^cfg80211'", check=False)
        check("wireless stack loaded at boot", rc == 0)
        rc, out = con.sh("systemctl is-active iwd", check=False)
        check("iwd running", rc == 0, out.splitlines()[-1] if out else "")
        version_a = con.sh("cat /etc/box-version")[1].splitlines()[-1]

        # 2. Install the update into slot B and boot it
        rc, out = con.sh("rauc install /data/update.raucb", timeout=1800, check=False)
        check("rauc install succeeds", rc == 0, out.splitlines()[-1] if out else "")
        check("B is now first in boot order", env(con, "BOOT_ORDER") == "B A",
              f"BOOT_ORDER={env(con, 'BOOT_ORDER')}")
        con.send("reboot\n")
        con.expect(r"Booting slot B", 900)
        con.login()
        check("update boots slot B", booted_slot(con) == "B")
        check("slot B marked good", env(con, "BOOT_B_LEFT") == "3",
              f"BOOT_B_LEFT={env(con, 'BOOT_B_LEFT')}")
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
    main()
