#!/usr/bin/env python3
"""Print input delivery receipts and focus events from a waywire desktop.

    receipts.py [--json] [--output FILE] [-- COMMAND [ARG...]]
    receipts.py --focus

The compositor sends a subscriber the receipts for input from its own process
tree, so run the driver under this tool (receipts.py -- cua-driver serve), or
use it to watch focus and window events, which every subscriber receives. With
a command, the tool exits when the command does, with its status. --focus
prints the focused window and its text-field state once.

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
    if member == "TextFieldChanged":
        return (
            f"#{serial} text field in {window_label(record.get('window'))}"
            f"{text_field_label(record)}"
        )
    return f"#{serial} {member} {record}"


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


def call(connection, method, timeout=5000):
    return connection.call_sync(
        NAME,
        PATH,
        NAME,
        method,
        None,
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

    def on_signal(_connection, _sender, _path, _interface, member, parameters):
        (record,) = parameters.unpack()
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
