"""Read the installed desktop's actual MIT-SCREEN-SAVER state once."""
import ctypes as c
import json


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
connection = x11.XOpenDisplay(b":1")
assert connection, "Cannot open the installed display :1"
try:
    info = SaverInfo()
    assert xss.XScreenSaverQueryInfo(connection, x11.XDefaultRootWindow(connection), c.byref(info))
    print(json.dumps({"state": info.state, "idle_ms": info.idle, "since_ms": info.since}), flush=True)
    assert info.state in (0, 3), "Desktop screen saver is active"
    assert info.idle >= 1800000, "Desktop received input during the thirty-minute test"
finally:
    x11.XCloseDisplay(connection)
