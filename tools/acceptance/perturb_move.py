#!/usr/bin/env python3
"""T6b interloper from the dogfood harness: drag a window's title bar through
the RemoteDesktop portal, as a person moving the window would.

Usage: perturb_move.py X1,Y1 X2,Y2 (run with the session's environment)
"""

import os
import sys
import time

from gi.repository import Gio, GLib

bus = Gio.bus_get_sync(Gio.BusType.SESSION, None)
sender = bus.get_unique_name()[1:].replace(".", "_")
PORTAL = ("org.freedesktop.portal.Desktop", "/org/freedesktop/portal/desktop")
RD = "org.freedesktop.portal.RemoteDesktop"
SC = "org.freedesktop.portal.ScreenCast"
requests = 0


def request(interface, method, build):
    global requests
    requests += 1
    token = f"pm{os.getpid()}_{requests}"
    path = f"/org/freedesktop/portal/desktop/request/{sender}/{token}"
    loop, box = GLib.MainLoop(), {}

    def on_response(_connection, _sender, _path, _interface, _signal, params):
        box["response"] = params.unpack()
        loop.quit()

    subscription = bus.signal_subscribe(
        PORTAL[0],
        "org.freedesktop.portal.Request",
        "Response",
        path,
        None,
        Gio.DBusSignalFlags.NO_MATCH_RULE,
        on_response,
    )
    bus.call_sync(PORTAL[0], PORTAL[1], interface, method, build(token), None, 0, 10000, None)
    GLib.timeout_add(10000, loop.quit)
    loop.run()
    bus.signal_unsubscribe(subscription)
    code, results = box["response"]
    if code != 0:
        raise SystemExit(f"{method} was refused: {code}")
    return results


def options(token, **extra):
    return {"handle_token": GLib.Variant("s", token), **extra}


session = request(
    RD,
    "CreateSession",
    lambda t: GLib.Variant(
        "(a{sv})", (options(t, session_handle_token=GLib.Variant("s", "s" + t)),)
    ),
)["session_handle"]
request(RD, "SelectDevices", lambda t: GLib.Variant("(oa{sv})", (session, options(t, types=GLib.Variant("u", 3)))))
request(SC, "SelectSources", lambda t: GLib.Variant("(oa{sv})", (session, options(t, types=GLib.Variant("u", 1)))))
streams = request(RD, "Start", lambda t: GLib.Variant("(osa{sv})", (session, "", options(t)))).get("streams", [])
node = streams[0][0] if streams else 0


def notify(method, signature, *args):
    bus.call_sync(
        PORTAL[0], PORTAL[1], RD, method,
        GLib.Variant(signature, (session, {}) + args), None, 0, 5000, None,
    )


x1, y1 = (float(v) for v in sys.argv[1].split(","))
x2, y2 = (float(v) for v in sys.argv[2].split(","))
notify("NotifyPointerMotionAbsolute", "(oa{sv}udd)", node, x1, y1)
time.sleep(0.1)
notify("NotifyPointerButton", "(oa{sv}iu)", 272, 1)
for step in range(1, 21):
    time.sleep(0.02)
    notify("NotifyPointerMotionAbsolute", "(oa{sv}udd)", node, x1 + (x2 - x1) * step / 20, y1 + (y2 - y1) * step / 20)
time.sleep(0.1)
notify("NotifyPointerButton", "(oa{sv}iu)", 272, 0)
bus.call_sync(PORTAL[0], session, "org.freedesktop.portal.Session", "Close", None, None, 0, 5000, None)
