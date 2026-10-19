#!/usr/bin/env python3
"""Serve update bundles to dev devices over the LAN.

RAUC streams bundles with HTTP Range requests, which `python3 -m http.server`
doesn't support. This is the same server plus single-range support.

Usage: tools/serve-updates.py [folder] [port]
       (default: /media/me/Claude/rpi-build/saved, port 8000)
On the device: rauc install http://<this computer's IP>:<port>/<bundle>.raucb
"""
import http.server
import os
import re
import socket
import sys

FOLDER = sys.argv[1] if len(sys.argv) > 1 else "/media/me/Claude/rpi-build/saved"
PORT = int(sys.argv[2]) if len(sys.argv) > 2 else 8000
RANGE = re.compile(r"bytes=(\d*)-(\d*)$")


class Handler(http.server.SimpleHTTPRequestHandler):
    def __init__(self, *args, **kwargs):
        super().__init__(*args, directory=FOLDER, **kwargs)

    def send_head(self):
        self._range = None
        header = self.headers.get("Range")
        path = self.translate_path(self.path)
        if not header or not os.path.isfile(path):
            return super().send_head()

        size = os.path.getsize(path)
        m = RANGE.match(header.strip())
        if not m or (not m.group(1) and not m.group(2)):
            self.send_error(416, "Only single byte ranges are supported")
            return None
        if m.group(1):
            start = int(m.group(1))
            end = min(int(m.group(2)), size - 1) if m.group(2) else size - 1
        else:  # suffix range: last N bytes
            start, end = max(size - int(m.group(2)), 0), size - 1
        if start >= size or start > end:
            self.send_response(416)
            self.send_header("Content-Range", f"bytes */{size}")
            self.end_headers()
            return None

        f = open(path, "rb")
        f.seek(start)
        self._range = end - start + 1
        self.send_response(206)
        self.send_header("Content-Type", self.guess_type(path))
        self.send_header("Accept-Ranges", "bytes")
        self.send_header("Content-Range", f"bytes {start}-{end}/{size}")
        self.send_header("Content-Length", str(self._range))
        self.send_header("Last-Modified", self.date_time_string(os.path.getmtime(path)))
        self.end_headers()
        return f

    def copyfile(self, source, outputfile):
        if self._range is None:
            return super().copyfile(source, outputfile)
        left = self._range
        while left > 0:
            chunk = source.read(min(64 * 1024, left))
            if not chunk:
                break
            outputfile.write(chunk)
            left -= len(chunk)

    def end_headers(self):
        # Advertise range support on full responses too (range ones set it above)
        if getattr(self, "_range", None) is None:
            self.send_header("Accept-Ranges", "bytes")
        super().end_headers()


def lan_addresses():
    s = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    try:
        s.connect(("192.0.2.1", 9))  # no packets sent; just picks the LAN interface
        return s.getsockname()[0]
    except OSError:
        return "<this computer's IP>"
    finally:
        s.close()


if __name__ == "__main__":
    ip = lan_addresses()
    print(f"Serving {FOLDER}")
    for name in sorted(os.listdir(FOLDER)):
        if name.endswith(".raucb"):
            print(f"  on the device: rauc install http://{ip}:{PORT}/{name}")
    print("Ctrl+C to stop.")
    http.server.ThreadingHTTPServer(("", PORT), Handler).serve_forever()
