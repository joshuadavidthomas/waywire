#!/usr/bin/env python3
"""Print input delivery receipts and focus events from a waywire desktop.

    receipts.py [--json] [--output FILE] [-- COMMAND [ARG...]]
    receipts.py --focus
    receipts.py --windows [--watch] [--json]
    receipts.py --settle [--quiet MS] [--timeout MS] [--window ID]
                [--serial N] [--json] [-- COMMAND [ARG...]]

The compositor sends a subscriber the receipts for input from its own process
tree, so run the driver under this tool (receipts.py -- cua-driver serve), or
use it to watch focus and window events, which every subscriber receives. With
a command, the tool exits when the command does, with its status. --focus
prints the focused window and its text-field state once. --windows prints
every window, topmost first, with its bounds on screen in output pixels (the
pixels of a full-screen screenshot), and with --watch prints the list again
each time it changes.

--settle waits until the screen stops changing and prints how long that took.
With a command, it runs the command, then waits from the command's first input
receipt: `receipts.py --settle -- xdotool key Return` times what one key
press redraws. With --serial, it waits from that receipt; with neither, from
now. --quiet sets how long the screen must stay unchanged (default 300 ms),
--window watches one window id from --windows instead of the whole screen,
and --timeout bounds the wait (default 5000 ms). It exits 0 when the screen
settled and 1 on a timeout, unless the command failed.

It connects to DBUS_SESSION_BUS_ADDRESS, the session bus the compositor
exports to its applications. The interface is described in
docs/input-delivery.md. Needs PyGObject (Debian python3-gi, Fedora
python3-gobject).
"""

import argparse
import json
import os
import signal
import subprocess
import sys

from gi.repository import Gio, GLib

NAME = "io.github.joshuadavidthomas.waywire.InputDelivery1"
PATH = "/io/github/joshuadavidthomas/waywire/InputDelivery1"


def window_label(window):
    if not window:
        return "nothing"
    title = window.get("title") or "?"
    details = [window.get("app_id") or ""]
    if window.get("pid"):
        details.append(f"pid {window['pid']}")
    if window.get("id"):
        details.append(f"id {window['id']}")
    return f'"{title}" [{", ".join(part for part in details if part)}]'


def text_field_label(record):
    field = record.get("text_field")
    if field is None:
        return ""
    if field == "focused":
        return f"; text field: {record.get('text_purpose', 'normal')}"
    if field == "none":
        return "; text field: none focused"
    return "; text field: unknown"


def box(rect):
    x, y, width, height = rect
    return f"{width}x{height}+{x}+{y}"


def window_lines(listing):
    """The window list as text: one line per window, topmost first."""
    lines = [
        f"output {listing['width']}x{listing['height']} scale {listing['scale']:g}"
        f", work area {box(listing['work_area'])}"
    ]
    for window in listing["windows"]:
        states = [
            name
            for name in ("focused", "minimized", "maximized", "fullscreen", "modal")
            if window.get(name)
        ]
        if window.get("parent"):
            states.append(f"dialog of {window['parent']}")
        states.append("x11" if window.get("x11") else "wayland")
        lines.append(
            f"  {window['stack']:>2} {box(window['geometry']):<20}"
            f" frame {box(window['frame']):<20} {window_label(window)}"
            f" {', '.join(states)}"
        )
    return lines


def describe(member, record):
    serial = record.get("serial", 0)
    if member == "Receipt":
        event = record["event"]
        if "pressed" in record:
            event += " down" if record["pressed"] else " up"
        source = record["source"]
        if record.get("source_pid"):
            source += f" pid {record['source_pid']}"
        delivery = record["delivery"]
        if delivery == "window":
            target = window_label(record.get("window"))
            if record.get("popup"):
                target += " (popup)"
        elif delivery == "compositor":
            target = f"compositor {record.get('chrome', '')} of {window_label(record.get('window'))}"
        elif delivery == "dropped":
            target = "DROPPED: a browser holds input control"
        else:
            target = "NOWHERE: no window took it"
        return f"#{serial} {event} from {source} -> {target}{text_field_label(record)}"
    if member == "FocusChanged":
        cause = record["cause"]
        if record.get("source"):
            cause += f" by {record['source']}"
        if record.get("receipt"):
            cause += f", receipt #{record['receipt']}"
        popup = " (popup)" if record.get("popup") else ""
        return (
            f"#{serial} focus -> {window_label(record.get('window'))}{popup}"
            f" from {window_label(record.get('previous'))}: {cause}"
        )
    if member == "WindowMapped":
        kind = "dialog" if record.get("parent") else "window"
        if record.get("modal"):
            kind = "modal " + kind
        parent = (
            f" over {window_label(record['parent'])}" if record.get("parent") else ""
        )
        return f"#{serial} new {kind} {window_label(record.get('window'))}{parent}"
    if member == "WindowChanged":
        return f"#{serial} window now {window_label(record.get('window'))}"
    if member == "WindowsChanged":
        return "\n".join([f"#{serial} windows:", *window_lines(record)[1:]])
    if member == "TextFieldChanged":
        return (
            f"#{serial} text field in {window_label(record.get('window'))}"
            f"{text_field_label(record)}"
        )
    return f"#{serial} {member} {record}"


def settle_line(answer):
    """A WaitSettled reply as text."""
    since = answer["input_usec"]
    changed = answer["damage_usec"]
    settle_ms = (max(changed, since) - since) / 1000
    waited_ms = (answer["time_usec"] - since) / 1000
    frames = f"{answer['frames']} frame{'s' if answer['frames'] != 1 else ''}"
    if answer["result"] == "settled":
        return (
            f"settled {settle_ms:.0f} ms after the input: {frames} changed the"
            f" screen, then it held still for {answer['quiet_ms']} ms"
            f" (answered after {waited_ms:.0f} ms)"
        )
    if answer["result"] == "timeout":
        last = f", last {settle_ms:.0f} ms after the input" if changed else ""
        return (
            f"TIMEOUT after {waited_ms:.0f} ms: {frames} changed the screen{last};"
            f" it never held still for {answer['quiet_ms']} ms"
        )
    return f"{answer['result'].upper()} after {waited_ms:.0f} ms: the window went away"


def connect():
    address = os.environ.get("DBUS_SESSION_BUS_ADDRESS")
    if not address:
        raise SystemExit("DBUS_SESSION_BUS_ADDRESS is not set")
    return Gio.DBusConnection.new_for_address_sync(
        address,
        Gio.DBusConnectionFlags.AUTHENTICATION_CLIENT
        | Gio.DBusConnectionFlags.MESSAGE_BUS_CONNECTION,
        None,
        None,
    )


def call(connection, method, timeout=5000, parameters=None):
    return connection.call_sync(
        NAME,
        PATH,
        NAME,
        method,
        parameters,
        None,
        Gio.DBusCallFlags.NONE,
        timeout,
        None,
    ).unpack()


def main():
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--json", action="store_true", help="print JSON lines")
    parser.add_argument(
        "--focus", action="store_true", help="print the current focus and exit"
    )
    parser.add_argument(
        "--windows", action="store_true", help="print the window list and exit"
    )
    parser.add_argument(
        "--watch",
        action="store_true",
        help="with --windows, print the list again whenever it changes",
    )
    parser.add_argument(
        "--settle",
        action="store_true",
        help="wait until the screen stops changing after the input, and print how long",
    )
    parser.add_argument(
        "--quiet", type=int, help="with --settle, the quiet window in ms (default 300)"
    )
    parser.add_argument(
        "--timeout", type=int, default=5000, help="with --settle, the longest wait in ms"
    )
    parser.add_argument("--window", help="with --settle, watch only this window id")
    parser.add_argument(
        "--serial", type=int, help="with --settle, wait from this receipt's serial"
    )
    parser.add_argument("--output", help="write to FILE instead of standard output")
    parser.add_argument("command", nargs="*", help="run COMMAND under the tool")
    args = parser.parse_args()
    out = open(args.output, "w", buffering=1) if args.output else sys.stdout
    connection = connect()
    if args.focus:
        (focus,) = call(connection, "GetFocus")
        if args.json:
            print(json.dumps(focus), file=out, flush=True)
        else:
            print(
                f"focus: {window_label(focus.get('window'))}{text_field_label(focus)}",
                file=out,
                flush=True,
            )
        return 0
    if args.windows:

        def show(listing):
            if args.json:
                print(json.dumps(listing), file=out, flush=True)
            else:
                print("\n".join(window_lines(listing)), file=out, flush=True)

        if args.watch:
            connection.signal_subscribe(
                None,
                NAME,
                "WindowsChanged",
                PATH,
                None,
                Gio.DBusSignalFlags.NONE,
                lambda *signal: show(signal[-1].unpack()[0]),
            )
            call(connection, "Subscribe")
        (listing,) = call(connection, "GetWindows")
        show(listing)
        if not args.watch:
            return 0
        loop = GLib.MainLoop()
        for number in (signal.SIGINT, signal.SIGTERM):
            GLib.unix_signal_add(GLib.PRIORITY_DEFAULT, number, loop.quit)
        loop.run()
        return 0

    serials = []

    def settle(serial):
        options = {"timeout_ms": GLib.Variant("u", args.timeout)}
        if serial:
            options["serial"] = GLib.Variant("t", serial)
        if args.quiet is not None:
            options["quiet_ms"] = GLib.Variant("u", args.quiet)
        if args.window:
            options["window"] = GLib.Variant("s", args.window)
        (answer,) = call(
            connection,
            "WaitSettled",
            args.timeout + 5000,
            GLib.Variant("(a{sv})", (options,)),
        )
        if args.json:
            print(json.dumps(answer), file=out, flush=True)
        else:
            print(settle_line(answer), file=out, flush=True)
        return 0 if answer["result"] == "settled" else 1

    if args.settle and not args.command:
        return settle(args.serial)

    def on_signal(_connection, _sender, _path, _interface, member, parameters):
        (record,) = parameters.unpack()
        if member == "Receipt":
            serials.append(record["serial"])
        if args.settle:
            return
        if args.json:
            print(json.dumps({"signal": member, **record}), file=out, flush=True)
        else:
            print(describe(member, record), file=out, flush=True)

    connection.signal_subscribe(
        None,
        NAME,
        None,
        PATH,
        None,
        Gio.DBusSignalFlags.NONE,
        on_signal,
    )
    call(connection, "Subscribe")
    loop = GLib.MainLoop()
    status = 0
    if args.command:
        child = subprocess.Popen(args.command)

        def child_exited(_pid, wait_status):
            nonlocal status
            status = os.waitstatus_to_exitcode(wait_status)
            # GetFocus replies after every signal for earlier input, so the
            # command's last receipts have arrived when it returns.
            if args.settle:
                call(connection, "GetFocus")
                # Run the signal handlers for receipts that arrived first.
                context = GLib.MainContext.default()
                while context.pending():
                    context.iteration(False)
                result = settle(serials[0] if serials else args.serial)
                if status == 0:
                    status = result
                loop.quit()
                return
            connection.call(
                NAME,
                PATH,
                NAME,
                "GetFocus",
                None,
                None,
                Gio.DBusCallFlags.NONE,
                5000,
                None,
                lambda *_: loop.quit(),
            )

        GLib.child_watch_add(GLib.PRIORITY_DEFAULT, child.pid, child_exited)
        for number in (signal.SIGINT, signal.SIGTERM):
            GLib.unix_signal_add(
                GLib.PRIORITY_DEFAULT, number, lambda n=number: child.send_signal(n)
            )
    else:
        for number in (signal.SIGINT, signal.SIGTERM):
            GLib.unix_signal_add(GLib.PRIORITY_DEFAULT, number, loop.quit)
    loop.run()
    return status if status >= 0 else 128 - status


if __name__ == "__main__":
    sys.exit(main())
