#!/usr/bin/env python3
"""A pointer driver on the RemoteDesktop portal, one command per input line.

    click X Y          move to (X, Y) and click the left button
    drag X1 Y1 X2 Y2   press at (X1, Y1), move in steps to (X2, Y2), release

Coordinates are the portal stream's logical coordinates. Each command prints
"ok" once the portal has accepted it. Run with the session's environment, and
under tools/receipts/receipts.py to record the receipts for its input.
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
    token = f"pp{os.getpid()}_{requests}"
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


def move(x, y):
    notify("NotifyPointerMotionAbsolute", "(oa{sv}udd)", node, x, y)


def button(pressed):
    notify("NotifyPointerButton", "(oa{sv}iu)", 272, 1 if pressed else 0)


print("ok", flush=True)
for line in sys.stdin:
    command, *values = line.split()
    numbers = [float(value) for value in values]
    if command == "click":
        move(*numbers)
        time.sleep(0.05)
        button(True)
        button(False)
    elif command == "drag":
        x1, y1, x2, y2 = numbers
        move(x1, y1)
        time.sleep(0.05)
        button(True)
        for step in range(1, 21):
            time.sleep(0.02)
            move(x1 + (x2 - x1) * step / 20, y1 + (y2 - y1) * step / 20)
        time.sleep(0.05)
        button(False)
    else:
        raise SystemExit(f"unknown command {command!r}")
    print("ok", flush=True)
bus.call_sync(PORTAL[0], session, "org.freedesktop.portal.Session", "Close", None, None, 0, 5000, None)
