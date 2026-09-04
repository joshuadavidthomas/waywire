"""Compare the v0 XFCE display's saver with a second Xvnc using -s 0.

Run only on sprite-desktop-v1-probe after v0 provisioning. This exec session
holds the VM active; this measures X screen idleness, not Sprite suspension.
"""
import ctypes as c
import json
import os
import subprocess
import time
from pathlib import Path


class SaverInfo(c.Structure):
    _fields_ = [
        ("window", c.c_ulong), ("state", c.c_int), ("kind", c.c_int),
        ("since", c.c_ulong), ("idle", c.c_ulong), ("event_mask", c.c_ulong),
    ]


x11 = c.CDLL("libX11.so.6")
xss = c.CDLL("libXss.so.1")
x11.XOpenDisplay.argtypes = [c.c_char_p]
x11.XOpenDisplay.restype = c.c_void_p
x11.XDefaultRootWindow.argtypes = [c.c_void_p]
x11.XDefaultRootWindow.restype = c.c_ulong
x11.XCloseDisplay.argtypes = [c.c_void_p]
xss.XScreenSaverQueryInfo.argtypes = [c.c_void_p, c.c_ulong, c.POINTER(SaverInfo)]
xss.XScreenSaverQueryInfo.restype = c.c_int


def query(display):
    connection = x11.XOpenDisplay(display.encode())
    if not connection:
        raise RuntimeError(f"Cannot open {display}")
    try:
        info = SaverInfo()
        if not xss.XScreenSaverQueryInfo(connection, x11.XDefaultRootWindow(connection), c.byref(info)):
            raise RuntimeError("MIT-SCREEN-SAVER extension is unavailable")
        return {"display": display, "state": info.state, "idle_ms": info.idle, "since_ms": info.since}
    finally:
        x11.XCloseDisplay(connection)


assert not Path("/tmp/.X2-lock").exists(), "Display :2 is occupied"
assert not Path("/tmp/desktop-m0-rfb.sock").exists(), "Probe socket is occupied"
process = subprocess.Popen([
    "Xvnc", ":2", "-geometry", "640x480", "-depth", "24", "-rfbport", "5901",
    "-localhost", "yes", "-SecurityTypes", "None", "-AlwaysShared", "-s", "0",
    "-rfbunixpath", "/tmp/desktop-m0-rfb.sock", "-rfbunixmode", "0600",
])
try:
    for _ in range(100):
        if Path("/tmp/.X11-unix/X2").is_socket():
            break
        if process.poll() is not None:
            raise RuntimeError("Probe Xvnc exited")
        time.sleep(0.1)
    subprocess.run(["xset", "-display", ":2", "s", "off"], check=True)
    subprocess.run(["xset", "-display", ":2", "-dpms"], check=True)
    subprocess.run(["xsetroot", "-display", ":2", "-solid", "#336699"], check=True)
    assert os.stat("/tmp/desktop-m0-rfb.sock").st_mode & 0o777 == 0o600
    for display in (":1", ":2"):
        subprocess.run(["xset", "-display", display, "q"], check=True)
    subprocess.run(["ss", "-ltn"], check=True)
    print(json.dumps({"phase": "before", "displays": [query(":1"), query(":2")]}), flush=True)
    time.sleep(900)
    after = [query(":1"), query(":2")]
    print(json.dumps({"phase": "after-15-minutes", "displays": after}), flush=True)
    # X11/extensions/saver.h: ScreenSaverOff = 0, ScreenSaverDisabled = 3.
    assert after[1]["state"] in (0, 3), "-s 0 and xset s off did not prevent blanking"
finally:
    process.terminate()
    try:
        process.wait(timeout=5)
    except subprocess.TimeoutExpired:
        process.kill()
        process.wait()
    Path("/tmp/desktop-m0-rfb.sock").unlink(missing_ok=True)
