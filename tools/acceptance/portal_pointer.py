#!/usr/bin/env python3
"""A pointer driver on the RemoteDesktop portal, one command per input line.

    click X Y          move to (X, Y) and click the left button
    drag X1 Y1 X2 Y2   press at (X1, Y1), move in steps to (X2, Y2), release
    settle QUIET TIMEOUT WINDOW FILE
                       wait until the screen, or window id WINDOW ("-" for
                       the whole screen), has held still for QUIET ms since
                       the last input, then save a grim screenshot to FILE

Coordinates are the portal stream's logical coordinates. Each command prints
"ok" once the portal has accepted it; settle prints "ok" and its WaitSettled
reply as JSON, with CLOCK_MONOTONIC microsecond times of the last click
(`clicked_usec`), the call (`called_usec`) and its return (`returned_usec`).
Run with the session's environment, and under tools/receipts/receipts.py to
record the receipts for its input.
"""

import json
import os
import subprocess
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


DELIVERY = "io.github.joshuadavidthomas.waywire.InputDelivery1"
DELIVERY_PATH = "/io/github/joshuadavidthomas/waywire/InputDelivery1"
# This process opened the portal session, so it receives its own receipts.
serials = []
# The latest serial when the previous settle returned; receipts after it
# belong to the input since.
settled_serial = 0
bus.signal_subscribe(
    None,
    DELIVERY,
    "Receipt",
    DELIVERY_PATH,
    None,
    Gio.DBusSignalFlags.NONE,
    lambda *signal: serials.append(signal[-1].unpack()[0]["serial"]),
)
bus.call_sync(DELIVERY, DELIVERY_PATH, DELIVERY, "Subscribe", None, None, 0, 5000, None)
clicked_usec = 0


def usec():
    return time.monotonic_ns() // 1000


def settle(quiet, timeout, window, path):
    global settled_serial
    # GetFocus replies after every receipt for earlier input; run their
    # handlers to learn the last one's serial.
    bus.call_sync(DELIVERY, DELIVERY_PATH, DELIVERY, "GetFocus", None, None, 0, 5000, None)
    context = GLib.MainContext.default()
    while context.pending():
        context.iteration(False)
    options = {
        "quiet_ms": GLib.Variant("u", int(quiet)),
        "timeout_ms": GLib.Variant("u", int(timeout)),
    }
    # Name the first receipt since the last wait, the click's press: the
    # wait also covers the rest of this process's input.
    since = [serial for serial in serials if serial > settled_serial]
    if since:
        options["serial"] = GLib.Variant("t", since[0])
    if window != "-":
        options["window"] = GLib.Variant("s", window)
    called = usec()
    (answer,) = bus.call_sync(
        DELIVERY, DELIVERY_PATH, DELIVERY, "WaitSettled",
        GLib.Variant("(a{sv})", (options,)), None, 0, int(timeout) + 5000, None,
    ).unpack()
    returned = usec()
    subprocess.run(["grim", "-t", "ppm", path], check=True)
    settled_serial = answer["serial"]
    return {
        **answer,
        "clicked_usec": clicked_usec,
        "called_usec": called,
        "returned_usec": returned,
    }


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
    if command == "settle":
        print("ok " + json.dumps(settle(*values)), flush=True)
        continue
    numbers = [float(value) for value in values]
    if command == "click":
        move(*numbers)
        time.sleep(0.05)
        button(True)
        button(False)
        clicked_usec = usec()
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
