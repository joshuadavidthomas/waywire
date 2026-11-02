#!/usr/bin/env python3
"""Run a stdio MCP computer-use driver behind waywire's input delivery checks.

    mcp_wrapper.py [--quiet-ms MS] [--settle-timeout-ms MS] [--trace FILE]
                   -- DRIVER [ARG...]

The wrapper starts the driver as its child and relays MCP between the client
on its standard input and output and the driver. The compositor sends a
subscriber the receipts for input from its own process tree
(docs/input-delivery.md, Privacy), so the wrapper sees every key, button and
text event the driver sends, and where each one went. The driver needs no
changes.

For each tool call that is not a query, the wrapper:

1. Waits for the call's receipts (GetFocus is the barrier), then calls
   WaitSettled from the call's first receipt, so the agent's next screenshot
   shows what the action led to without a sleep. --quiet-ms and
   --settle-timeout-ms are passed through as `quiet_ms` and `timeout_ms`.
2. Checks every key and text receipt. The call fails, with `isError` and a
   first content block that starts with `INPUT DELIVERY CHECK FAILED`, when
   keys reached another window than the agent directed them to, reached no
   window, were dropped while a person held input control, went to a window
   whose text field lost focus without input from the agent (an in-window
   dialog such as a page alert()), or, for typing calls, reached no focused
   text field. The input has already been delivered when this is reported.
3. Appends an `input_delivery:` text block: where the input went, focus
   changes, the focused window's bounds on screen, and the settle result.

It adds two tools: `get_windows`, the GetWindows list with each window's
bounds in screenshot pixels, and `wait_settled`, for a longer quiet window.
Tool calls run one at a time. On a desktop without the interface the wrapper
relays everything unchanged.

--trace writes every signal, call and verdict as JSON lines, for evidence and
for the tests in test_mcp_wrapper.py. Typed text is not written. Needs
PyGObject (Debian python3-gi, Fedora python3-gobject).
"""

import argparse
import json
import os
import queue
import re
import subprocess
import sys
import threading
import time
from dataclasses import dataclass, field

NAME = "io.github.joshuadavidthomas.waywire.InputDelivery1"
PATH = "/io/github/joshuadavidthomas/waywire/InputDelivery1"

# A text field that turns `none` within this long after one of the agent's own
# inputs was moved by that input; later, by something else, such as an alert().
EXPLAINED_US = 150_000
# Tools that only read, for drivers that do not mark them `readOnlyHint`.
QUERY_NAMES = re.compile(r"^(screenshot|zoom|get_|list_|focused_window$|doctor$)")
# Arguments that name the window a driver call acts on.
SELECTORS = ("pid", "app_id", "wm_class", "title")
FAILED = "INPUT DELIVERY CHECK FAILED: "

LOCAL_TOOLS = {
    "get_windows": {
        "name": "get_windows",
        "description": (
            "List every window, topmost first, as the compositor draws them: id, title,"
            " app_id, pid, geometry (the content as [x, y, width, height] in the pixels of"
            " a full-screen screenshot), frame (geometry plus the compositor's titlebar),"
            " stack (0 is the top), focused, minimized, maximized, fullscreen, parent and"
            " modal. To click a window, click inside its geometry at a point that no"
            " window with a lower stack covers."
        ),
        "inputSchema": {"type": "object", "properties": {}},
        "annotations": {"readOnlyHint": True},
    },
    "wait_settled": {
        "name": "wait_settled",
        "description": (
            "Wait until the screen, or one window id from get_windows, has not changed"
            " for quiet_ms. Every action already waits this way before it returns; call"
            " this for a longer quiet window, such as when an application redraws after"
            " a delay."
        ),
        "inputSchema": {
            "type": "object",
            "properties": {
                "quiet_ms": {"type": "integer", "minimum": 10, "maximum": 10000},
                "timeout_ms": {"type": "integer", "minimum": 0, "maximum": 20000},
                "window": {"type": "string", "description": "A window id from get_windows"},
            },
        },
        "annotations": {"readOnlyHint": True},
    },
}
WINDOW_KEYS = (
    "id", "title", "app_id", "pid", "x11", "geometry", "frame", "stack", "focused",
    "minimized", "maximized", "fullscreen", "parent", "modal",
)


def label(window):
    if not window:
        return "no window"
    title = window.get("title") or "untitled"
    return f'"{title}" ({window.get("app_id") or "?"}, id {window.get("id") or "?"})'


def same(a, b):
    if not a or not b:
        return False
    if a.get("id") or b.get("id"):
        return a.get("id") == b.get("id")
    return a.get("pid") == b.get("pid") and a.get("title") == b.get("title")


def short_app_id(value):
    value = (value or "").strip().lower()
    value = value.removesuffix(".desktop")
    return value.rsplit(".", 1)[-1]


def is_query(name, annotations, arguments):
    """Whether a call only reads, so it needs no settle and no check."""
    if name in LOCAL_TOOLS:
        return True
    if annotations and annotations.get("readOnlyHint"):
        return True
    action = arguments.get("action") if isinstance(arguments, dict) else None
    if isinstance(action, str) and QUERY_NAMES.match(action):
        return True
    return bool(QUERY_NAMES.match(name))


def is_typing(name, arguments):
    """Whether a call types text, as opposed to pressing keys."""
    action = arguments.get("action") if isinstance(arguments, dict) else None
    return "type" in name or action == "type"


@dataclass
class Call:
    tool: str
    arguments: dict
    typing: bool
    keys: int = 0
    buttons: int = 0
    nowhere: int = 0
    dropped: int = 0
    wrong: dict = field(default_factory=dict)  # window label -> keys
    blind: dict = field(default_factory=dict)  # window label -> keys
    fieldless: dict = field(default_factory=dict)  # window label -> keys
    seen_focused: bool = False
    delivered: dict = field(default_factory=dict)  # window label -> events
    first_serial: int = 0
    focus_moves: list = field(default_factory=list)
    moved: bool = False


@dataclass
class Report:
    failures: list
    notes: list
    summary: dict


class Checker:
    """Decides, from the compositor's records, whether a call's input went
    where the agent directed it. It sees records in serial order, and the
    start and end of each call; it does no I/O, so recorded streams replay
    through it unchanged."""

    def __init__(self, focus, listing=None):
        self.expected = (focus or {}).get("window") or None
        self.listing = listing
        self.windows = {}  # id -> latest window record
        self.fields = {}  # window id -> (text-field state, explained by the agent)
        self.fresh = set()  # windows whose field turned `none` unexplained since the last call
        self.own_times = []
        self.surprise = {}  # window id -> the focus change that moved focus there
        self.call = None
        for window in (listing or {}).get("windows", []):
            self._remember(window)

    def _remember(self, window):
        if window and window.get("id"):
            self.windows[window["id"]] = {**self.windows.get(window["id"], {}), **window}

    def _current(self, window):
        """The window with its latest known title: records are snapshots."""
        if window and window.get("id") in self.windows:
            return {**window, **self.windows[window["id"]]}
        return window

    def _directed(self, window):
        """The agent's own input put focus on this window."""
        self.expected = window
        self.surprise.pop((window or {}).get("id"), None)

    def begin(self, tool, arguments, typing):
        self.call = Call(tool, arguments if isinstance(arguments, dict) else {}, typing)

    def observe(self, record):
        kind = record.get("signal")
        for key in ("window", "previous"):
            self._remember(record.get(key))
        if kind == "WindowsChanged":
            self.listing = record
            for window in record.get("windows", []):
                self._remember(window)
        elif kind == "Receipt":
            self._receipt(record)
        elif kind == "FocusChanged":
            self._focus(record)
        elif kind == "TextFieldChanged":
            when = record.get("time_usec", 0)
            explained = record.get("text_field") != "none" or any(
                0 <= when - own <= EXPLAINED_US for own in self.own_times
            )
            wid = (record.get("window") or {}).get("id")
            self.fields[wid] = (record.get("text_field"), explained)
            if explained:
                self.fresh.discard(wid)
            else:
                self.fresh.add(wid)

    def _focus(self, record):
        window = record.get("window") or None
        call = self.call
        if call is not None:
            call.focus_moves.append(record)
            call.moved = True
        if record.get("receipt"):
            # The agent's own click or key moved focus.
            self._directed(window)
        elif call is not None and record.get("cause") == "window-request":
            # The driver activated a window for this call.
            self._directed(window)
        elif record.get("cause") in ("window-closed", "minimized") and same(
            record.get("previous"), self.expected
        ):
            # The window the agent was using went away; nothing else is meant.
            self._directed(window)
        elif window:
            self.surprise[window.get("id")] = record

    def _receipt(self, record):
        call = self.call
        if call is None:
            return
        self.own_times = (self.own_times + [record.get("time_usec", 0)])[-64:]
        if not call.first_serial:
            call.first_serial = record.get("serial", 0)
        if not record.get("pressed", True):
            return
        delivery = record.get("delivery")
        window = self._current(record.get("window") or None)
        if delivery == "dropped":
            call.dropped += 1
        if record.get("event") == "button":
            call.buttons += 1
            if delivery == "window" and window:
                call.delivered[label(window)] = call.delivered.get(label(window), 0) + 1
            return
        if record.get("event") not in ("key", "text"):
            return
        call.keys += 1
        if delivery == "nowhere":
            call.nowhere += 1
            return
        if delivery != "window":
            return
        where = label(window)
        call.delivered[where] = call.delivered.get(where, 0) + 1
        selector = {k: call.arguments[k] for k in SELECTORS if call.arguments.get(k)}
        if selector and not self._matches(selector, window):
            call.wrong[where] = call.wrong.get(where, 0) + 1
            return
        if not selector and self.expected and window.get("id") and not same(
            window, self.expected
        ):
            call.wrong[where] = call.wrong.get(where, 0) + 1
            return
        state = record.get("text_field")
        if state == "focused":
            call.seen_focused = True
        if state != "none":
            return
        recorded = self.fields.get(window.get("id"))
        if recorded and recorded[0] == "none" and not recorded[1]:
            call.blind[where] = call.blind.get(where, 0) + 1
        elif call.typing:
            call.fieldless[where] = call.fieldless.get(where, 0) + 1

    def _matches(self, selector, window):
        if selector.get("pid") and window.get("pid"):
            try:
                if int(selector["pid"]) != window["pid"]:
                    return False
            except (TypeError, ValueError):
                pass
        for key in ("app_id", "wm_class"):
            if selector.get(key) and short_app_id(selector[key]) != short_app_id(
                window.get("app_id")
            ):
                return False
        if selector.get("title"):
            titles = {window.get("title") or ""}
            titles.add(self.windows.get(window.get("id"), {}).get("title") or "")
            if not any(str(selector["title"]).lower() in t.lower() for t in titles):
                return False
        return True

    def finish(self, focus, settle):
        """End the call: its verdict, given GetFocus and WaitSettled after it."""
        call, self.call = self.call, None
        focus = focus or {}
        focused = self._current(focus.get("window") or None)
        failures = []
        keys = call.keys
        if call.dropped:
            failures.append(
                f"{call.dropped} input events were dropped: a person holds input control"
                " from the viewer."
            )
        if call.nowhere:
            failures.append(
                f"{call.nowhere} of {keys} keys reached no window: nothing had keyboard focus."
            )
        for where, count in call.wrong.items():
            target = (
                "the window this call names"
                if any(call.arguments.get(k) for k in SELECTORS)
                else label(self._current(self.expected))
            )
            reason = ""
            moved = next(
                (
                    record
                    for record in self.surprise.values()
                    if label(self._current(record.get("window"))) == where
                ),
                None,
            )
            if moved:
                reason = f" It took focus by itself ({moved.get('cause')}), not by your input."
            failures.append(f"{count} of {keys} keys went to {where}, not to {target}.{reason}")
        for where, count in call.blind.items():
            failures.append(
                f"{count} of {keys} keys went to {where} after its text field lost focus"
                " without input from you: a dialog inside the window, such as a page"
                " alert(), may have taken them."
            )
        if not call.seen_focused:
            # Text may tab from a field to a button on purpose; text that
            # reached no field at all went nowhere useful.
            for where, count in call.fieldless.items():
                failures.append(
                    f"{count} of {keys} typed keys went to {where}, which had no focused"
                    " text field."
                )

        notes = []
        moves = []
        for record in call.focus_moves:
            line = f"{label(self._current(record.get('window')))} ({record.get('cause')})"
            moves.append(line)
            if not record.get("receipt") and record.get("cause") not in (
                "click", "window-request"
            ):
                notes.append(f"Focus moved to {line} during this call, not by your input.")
        if not call.blind:
            for wid in sorted(self.fresh, key=str):
                notes.append(
                    f"The text field in {label(self.windows.get(wid))} lost focus without"
                    " input from you; a dialog inside the window may have opened."
                )
        self.fresh.clear()
        settled = None
        if settle:
            settle_ms = 0
            if settle.get("damage_usec"):
                settle_ms = round((settle["damage_usec"] - settle["input_usec"]) / 1000)
            settled = {"result": settle.get("result"), "ms": settle_ms}
            if settle.get("result") == "timeout":
                waited = round((settle["time_usec"] - settle["input_usec"]) / 1000)
                notes.append(
                    f"The screen was still changing {waited} ms after the input"
                    f" ({settle.get('frames', 0)} changed frames); a screenshot now may show"
                    " an intermediate state. Call wait_settled to wait longer."
                )
            elif settle.get("result") == "closed":
                notes.append("The watched window closed while the screen settled.")

        if failures or (call.moved and not call.keys and not call.buttons):
            # After a failure the agent has been told where focus is; a call
            # that sent no input, such as launching or activating a window,
            # moved focus on purpose. Later keys are checked against it.
            self.expected = focused
            self.surprise.clear()
        if failures:
            self.fields = {
                wid: (state, True) for wid, (state, _) in self.fields.items()
            }

        summary = {
            "keys": keys,
            "buttons": call.buttons,
            "delivered_to": call.delivered,
            "focus": label(focused),
            "text_field": focus.get("text_field"),
        }
        bounds = self._bounds(focused)
        if bounds:
            summary["focus_geometry"] = bounds
        if moves:
            summary["focus_changes"] = moves
        if settled:
            summary["settle"] = settled
        if notes:
            summary["notes"] = notes
        return Report(failures, notes, summary)

    def _bounds(self, window):
        for entry in (self.listing or {}).get("windows", []):
            if window and entry.get("id") == window.get("id"):
                return entry.get("geometry")
        return None

    def first_serial(self):
        return self.call.first_serial if self.call else 0


def annotate(result, report, overhead_ms):
    """The driver's MCP result with the verdict added."""
    summary = dict(report.summary, overhead_ms=overhead_ms)
    content = list(result.get("content") or [])
    content.append({"type": "text", "text": "input_delivery: " + json.dumps(summary)})
    if report.failures:
        content.insert(
            0,
            {
                "type": "text",
                "text": FAILED
                + " ".join(report.failures)
                + " The input was delivered as described: look at the screen before you"
                " retry, and do not type it again blindly.",
            },
        )
        return dict(result, content=content, isError=True)
    return dict(result, content=content)


class Bus:
    """The InputDelivery1 interface on the session bus. Signals are dispatched
    only when drain() runs, on the thread that created the Bus, so they reach
    the checker in order, before the reply to any later call is used."""

    def __init__(self, on_signal):
        from gi.repository import Gio, GLib

        self.GLib = GLib
        self.context = GLib.MainContext.new()
        self.context.push_thread_default()
        address = os.environ.get("DBUS_SESSION_BUS_ADDRESS")
        if not address:
            raise RuntimeError("DBUS_SESSION_BUS_ADDRESS is not set")
        self.connection = Gio.DBusConnection.new_for_address_sync(
            address,
            Gio.DBusConnectionFlags.AUTHENTICATION_CLIENT
            | Gio.DBusConnectionFlags.MESSAGE_BUS_CONNECTION,
            None,
            None,
        )
        self.Gio = Gio
        (version,) = self.connection.call_sync(
            NAME, PATH, "org.freedesktop.DBus.Properties", "Get",
            GLib.Variant("(ss)", (NAME, "Version")), None, Gio.DBusCallFlags.NONE,
            5000, None,
        ).unpack()
        self.version = version
        self.connection.signal_subscribe(
            NAME, NAME, None, PATH, None, Gio.DBusSignalFlags.NONE,
            lambda _c, _s, _p, _i, member, params: on_signal(
                {"signal": member, **params.unpack()[0]}
            ),
        )
        self.call("Subscribe")

    def call(self, method, options=None, timeout=10000):
        parameters = None
        if options is not None:
            parameters = self.GLib.Variant("(a{sv})", (options,))
        reply = self.connection.call_sync(
            NAME, PATH, NAME, method, parameters, None, self.Gio.DBusCallFlags.NONE,
            timeout, None,
        ).unpack()
        return reply[0] if reply else None

    def drain(self):
        while self.context.pending():
            self.context.iteration(False)

    def wait_settled(self, serial=0, quiet_ms=None, timeout_ms=5000, window=None):
        GLib = self.GLib
        options = {"timeout_ms": GLib.Variant("u", timeout_ms)}
        if serial:
            options["serial"] = GLib.Variant("t", serial)
        if quiet_ms is not None:
            options["quiet_ms"] = GLib.Variant("u", quiet_ms)
        if window:
            options["window"] = GLib.Variant("s", window)
        return self.call("WaitSettled", options, timeout_ms + 5000)


class Wrapper:
    """Relays MCP between the client and the driver; checks each tool call."""

    def __init__(self, command, quiet_ms, timeout_ms, trace):
        self.quiet_ms = quiet_ms
        self.timeout_ms = timeout_ms
        self.trace = trace
        self.inbox = queue.Queue()
        self.annotations = {}  # tool name -> annotations from tools/list
        self.local = set(LOCAL_TOOLS)
        self.lists = set()  # ids of tools/list requests
        self.waiting = []  # tools/call requests not started yet
        self.active = None  # (request, checked, started)
        self.bus = None
        self.checker = None
        try:
            self.bus = Bus(self._signal)
            focus = self.bus.call("GetFocus")
            listing = self.bus.call("GetWindows") if self.bus.version >= 2 else None
            self.checker = Checker(focus, listing)
            self._trace(event="start", version=self.bus.version, focus=focus, windows=listing)
        except Exception as error:  # Not waywire: relay unchanged.
            print(f"mcp_wrapper: input delivery unavailable, relaying only: {error}",
                  file=sys.stderr, flush=True)
            self.bus = None
        self.driver = subprocess.Popen(command, stdin=subprocess.PIPE, stdout=subprocess.PIPE)

    def _trace(self, **event):
        if self.trace:
            self.trace.write(json.dumps(event) + "\n")
            self.trace.flush()

    def _signal(self, record):
        self._trace(event="signal", record=record)
        self.checker.observe(record)

    def run(self):
        threading.Thread(target=self._read, args=(sys.stdin.buffer, "client"), daemon=True).start()
        threading.Thread(target=self._read, args=(self.driver.stdout, "driver"), daemon=True).start()
        while True:
            try:
                source, line = self.inbox.get(timeout=0.5)
            except queue.Empty:
                if self.bus:
                    self.bus.drain()
                continue
            if source == "driver" and line is None:
                break
            if source == "client" and line is None:
                self.driver.stdin.close()
                continue
            try:
                message = json.loads(line)
            except ValueError:
                message = None
            if not isinstance(message, dict):
                self._write(source, line)
            elif source == "client":
                self._from_client(message, line)
            else:
                self._from_driver(message, line)
        return self.driver.wait()

    def _read(self, stream, source):
        for line in stream:
            self.inbox.put((source, line))
        self.inbox.put((source, None))

    def _write(self, source, line):
        """Send a line on to the other side."""
        out = self.driver.stdin if source == "client" else sys.stdout.buffer
        try:
            out.write(line)
            out.flush()
        except (BrokenPipeError, ValueError):
            pass

    def _send(self, message):
        self._write("driver", (json.dumps(message) + "\n").encode())

    def _from_client(self, message, line):
        method = message.get("method")
        if not self.bus or "id" not in message or method is None:
            if method == "notifications/cancelled":
                if self._cancel(message.get("params", {}).get("requestId")):
                    return
            self._write("client", line)
            return
        if method == "tools/list" and not (message.get("params") or {}).get("cursor"):
            self.lists.add(message["id"])
        if method == "tools/call":
            self.waiting.append((message, line))
            self._next()
            return
        self._write("client", line)

    def _cancel(self, request_id):
        """Drop a cancelled call; True when it never reached the driver."""
        for index, (message, _) in enumerate(self.waiting):
            if message.get("id") == request_id:
                del self.waiting[index]
                return True
        if self.active and self.active[0].get("id") == request_id:
            if self.active[1]:
                focus = self.bus.call("GetFocus")
                self.bus.drain()
                self.checker.finish(focus, None)
            self.active = None
            self._write("client", (json.dumps({
                "jsonrpc": "2.0", "method": "notifications/cancelled",
                "params": {"requestId": request_id},
            }) + "\n").encode())
            self._next()
            return True
        return False

    def _next(self):
        while self.active is None and self.waiting:
            message, line = self.waiting.pop(0)
            params = message.get("params") or {}
            name = params.get("name", "")
            arguments = params.get("arguments") or {}
            if name in self.local:
                self._send({"jsonrpc": "2.0", "id": message["id"], "result": self._local(name, arguments)})
                continue
            checked = not is_query(name, self.annotations.get(name), arguments)
            self.bus.drain()
            if checked:
                typing = is_typing(name, arguments)
                selectors = {k: arguments[k] for k in SELECTORS if k in arguments}
                if isinstance(arguments, dict) and "action" in arguments:
                    selectors["action"] = arguments["action"]
                self._trace(event="call", tool=name, arguments=selectors, typing=typing)
                self.checker.begin(name, arguments, typing)
            self.active = (message, checked, time.monotonic())
            self._write("client", line)

    def _from_driver(self, message, line):
        if "method" in message or "id" not in message:
            self._write("driver", line)
            return
        if message["id"] in self.lists and "result" in message:
            self.lists.discard(message["id"])
            tools = list(message["result"].get("tools") or [])
            for tool in tools:
                self.annotations[tool.get("name")] = tool.get("annotations") or {}
            names = {tool.get("name") for tool in tools}
            self.local = {name for name in LOCAL_TOOLS if name not in names}
            if self.bus.version < 2:
                self.local.discard("get_windows")
            if self.bus.version < 3:
                self.local.discard("wait_settled")
            tools += [LOCAL_TOOLS[name] for name in sorted(self.local)]
            message["result"] = dict(message["result"], tools=tools)
            self._send(message)
            return
        if not self.active or self.active[0].get("id") != message["id"]:
            self._write("driver", line)
            return
        _, checked, started = self.active
        self.active = None
        if checked:
            replied = time.monotonic()
            self.bus.call("GetFocus")
            self.bus.drain()
            settle = None
            if self.bus.version >= 3:
                settle = self.bus.wait_settled(
                    self.checker.first_serial(), self.quiet_ms, self.timeout_ms
                )
            focus = self.bus.call("GetFocus")
            self.bus.drain()
            report = self.checker.finish(focus, settle)
            overhead_ms = round((time.monotonic() - replied) * 1000)
            self._trace(
                event="finish", focus=focus, settle=settle, failures=report.failures,
                summary=report.summary, overhead_ms=overhead_ms,
                call_ms=round((replied - started) * 1000),
            )
            if "result" in message:
                message["result"] = annotate(message["result"], report, overhead_ms)
                self._send(message)
            else:
                self._write("driver", line)
        else:
            self._write("driver", line)
        self._next()

    def _local(self, name, arguments):
        try:
            if name == "get_windows":
                listing = self.bus.call("GetWindows")
                self.bus.drain()
                text = {
                    "width": listing["width"],
                    "height": listing["height"],
                    "scale": listing["scale"],
                    "work_area": listing["work_area"],
                    "windows": [
                        {key: window.get(key) for key in WINDOW_KEYS if key in window}
                        for window in listing["windows"]
                    ],
                }
            else:
                started = time.monotonic()
                answer = self.bus.wait_settled(
                    0,
                    arguments.get("quiet_ms", self.quiet_ms),
                    arguments.get("timeout_ms", self.timeout_ms),
                    arguments.get("window"),
                )
                self.bus.drain()
                text = {
                    "result": answer["result"],
                    "waited_ms": round((time.monotonic() - started) * 1000),
                    "frames": answer["frames"],
                    "quiet_ms": answer["quiet_ms"],
                }
            return {"content": [{"type": "text", "text": json.dumps(text)}]}
        except Exception as error:
            return {"isError": True, "content": [{"type": "text", "text": f"{name}: {error}"}]}


def main():
    parser = argparse.ArgumentParser(
        description=__doc__.splitlines()[0],
        usage="%(prog)s [--quiet-ms MS] [--settle-timeout-ms MS] [--trace FILE] -- DRIVER [ARG...]",
    )
    parser.add_argument(
        "--quiet-ms", type=int,
        help="WaitSettled's quiet window after each action (default: the compositor's, 300)",
    )
    parser.add_argument(
        "--settle-timeout-ms", type=int, default=5000,
        help="the longest wait for the screen to settle after an action (default 5000)",
    )
    parser.add_argument("--trace", help="append every signal, call and verdict to FILE")
    parser.add_argument("command", nargs=argparse.REMAINDER)
    args = parser.parse_args()
    command = args.command[1:] if args.command[:1] == ["--"] else args.command
    if not command:
        parser.error("name the driver command after --")
    trace = open(args.trace, "a") if args.trace else None
    return Wrapper(command, args.quiet_ms, args.settle_timeout_ms, trace).run()


if __name__ == "__main__":
    sys.exit(main())
