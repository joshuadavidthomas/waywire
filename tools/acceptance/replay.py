#!/usr/bin/env python3
"""Replay the dogfood tasks' recorded driver calls against fresh waywire sessions.

Runs inside the acceptance image, started by suite.py. Usage:
  replay.py --out DIR [CELL ...]   run cells (all by default), one fresh session each
  replay.py --list                 print the cells

A cell is one task replayed through one driver. Its calls are the ones a passing
dogfood run made, in order; `rows` names them in the dogfood call log. The
verdict comes only from ground truth: task-server submissions, files on disk,
and compositor state read by a separate Wayland client. A driver's own success
reports never count. Coordinates of page elements come from the page's element
boxes; other coordinates are the recorded ones.

Known failures are cells whose check fails on main because of a known bug. They
report XFAIL; if one passes, it reports XPASS and the run fails, so the marker
gets removed together with the fix.
"""

import argparse
import hashlib
import json
import os
import re
import select
import shutil
import signal
import subprocess
import sys
import time
from dataclasses import dataclass, field
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
HERE = REPO / "tools/acceptance"
BIN = REPO / "target/release"
HOME = Path("/home/user")
WORK = HOME / "df/work"
SCRATCH = Path("/tmp/cell")
TASKS = "http://127.0.0.1:18950"
DISTRO = next(
    line.split("=", 1)[1].strip()
    for line in Path("/etc/os-release").read_text().splitlines()
    if line.startswith("VERSION_CODENAME=")
)
# Debian 12's Xwayland 22.1 names each window's wl_surface with the WL_SURFACE_ID
# client message. Smithay records the id but leaves matching it to the compositor,
# and waywire never does, so X11 windows map without a surface and are never drawn.
# Xwayland 23.1 and later use xwayland_shell_v1, which Smithay handles.
OLD_XWAYLAND = (
    "Xwayland 22.1 (Debian 12) associates surfaces by WL_SURFACE_ID, which waywire never "
    "matches, so X11 windows are never drawn (crates/compositor/src/xwayland.rs; "
    "fix in progress on branch xwayland-22)"
)
DRIVERS = {
    "cua": (
        ["/opt/cua/cua-driver", "mcp"],
        {"CUA_DRIVER_RS_ENABLE_WAYLAND": "1", "XDG_SESSION_TYPE": "wayland"},
    ),
    "cul-stock": (
        ["/opt/drivers/cul-stock", "mcp"],
        {"XDG_SESSION_TYPE": "wayland", "COMPUTER_USE_LINUX_INDICATOR": "0"},
    ),
    "cul-patched": (
        ["/opt/drivers/cul-patched", "mcp"],
        {"XDG_SESSION_TYPE": "wayland", "COMPUTER_USE_LINUX_INDICATOR": "0"},
    ),
    "x11demo": (["/opt/x11-loop/bin/python", str(HERE / "x11_loop_mcp.py")], {}),
}


def wait_for(predicate, message, timeout=30):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        value = predicate()
        if value:
            return value
        time.sleep(0.1)
    raise RuntimeError(message)


def kill_everything_else():
    """Leave only the container's init and this runner, so each cell starts clean."""
    keep = {1, os.getpid()}
    for _ in range(50):
        alive = [int(p) for p in os.listdir("/proc") if p.isdigit() and int(p) not in keep]
        alive = [pid for pid in alive if Path(f"/proc/{pid}/stat").exists()]
        alive = [pid for pid in alive if Path(f"/proc/{pid}/stat").read_text().split()[2] != "Z"]
        if not alive:
            return
        for pid in alive:
            try:
                os.kill(pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
        try:
            while os.waitpid(-1, os.WNOHANG)[0]:
                pass
        except ChildProcessError:
            pass
        time.sleep(0.1)
    raise RuntimeError("processes from the previous cell survived SIGKILL")


class Session:
    """A fresh gateway, compositor, private bus and task server, as in the dogfood run."""

    def __init__(self, out):
        self.out = out
        runtime = SCRATCH / "runtime"
        runtime.mkdir(mode=0o700, parents=True)
        WORK.mkdir(parents=True)
        # Xwayland's socket directory, which a container does not provide.
        Path("/tmp/.X11-unix").mkdir(exist_ok=True)
        os.chmod("/tmp/.X11-unix", 0o1777)
        base = {
            "HOME": str(HOME),
            "USER": "user",
            "LANG": "C.UTF-8",
            "PATH": os.environ["PATH"],
            "XDG_RUNTIME_DIR": str(runtime),
        }
        self.server = subprocess.Popen(
            [sys.executable, str(HERE / "task_server.py"), str(out)],
            stdout=subprocess.DEVNULL,
            stderr=open(out / "task-server.log", "w"),
        )
        env_file = runtime / "session.env"
        self.gateway = subprocess.Popen(
            [
                "dbus-run-session",
                "--",
                str(BIN / "waywire-gateway"),
                "--listen",
                "127.0.0.1:18900",
                "--public-url",
                "http://127.0.0.1:18900",
                "--resolution",
                "1280x720",
                "--compositor",
                str(BIN / "waywire-compositor"),
                "--",
                "/bin/sh",
                "-c",
                f"umask 077; env -0 > {env_file}.part; mv {env_file}.part {env_file}; exec foot --title=Terminal",
            ],
            env=base,
            stdout=open(out / "session.log", "w"),
            stderr=subprocess.STDOUT,
            start_new_session=True,
        )
        wait_for(env_file.exists, "the session did not start")
        self.env = dict(
            item.split("=", 1)
            for item in env_file.read_bytes().decode().split("\0")
            if "=" in item
        )
        wait_for(lambda: self.window("Terminal"), "the session's Terminal did not map")
        wait_for(self.task_server_up, "the task server did not start")
        # In the dogfood run the drivers' MCP servers outlived every task, so
        # accessibility was already on when each task's apps started. Turn it on
        # before launching apps, as there.
        self.run(
            sys.executable,
            "-c",
            "from gi.repository import Gio, GLib; "
            "Gio.bus_get_sync(Gio.BusType.SESSION).call_sync('org.a11y.Bus', '/org/a11y/bus', "
            "'org.freedesktop.DBus.Properties', 'Set', "
            "GLib.Variant('(ssv)', ('org.a11y.Status', 'IsEnabled', GLib.Variant('b', True))), "
            "None, 0, 5000, None)",
        )

    def task_server_up(self):
        return subprocess.run(
            ["python3", "-c", f"import urllib.request; urllib.request.urlopen('{TASKS}/form')"],
            stderr=subprocess.DEVNULL,
            check=False,
        ).returncode == 0

    def launch(self, name, args, **extra):
        return subprocess.Popen(
            args,
            env=self.env | extra,
            stdin=subprocess.DEVNULL,
            stdout=open(self.out / f"app-{name}.log", "w"),
            stderr=subprocess.STDOUT,
            start_new_session=True,
        )

    def run(self, *args, **extra):
        return subprocess.run(
            args, env=self.env | extra, capture_output=True, text=True, check=True
        ).stdout

    def toplevels(self):
        return json.loads(self.run(sys.executable, str(HERE / "wl_probe.py"), "toplevels"))

    def window(self, pattern):
        """The first mapped toplevel whose title matches, from the compositor."""
        return next(
            (w for w in self.toplevels() if re.search(pattern, w["title"])), None
        )

    def wait_window(self, pattern, timeout=30):
        return wait_for(
            lambda: self.window(pattern), f"no window titled {pattern!r}", timeout
        )

    def frame(self):
        """The output as (width, height, RGB bytes), captured with grim."""
        data = subprocess.run(
            ["grim", "-t", "ppm", "-"], env=self.env, capture_output=True, check=True
        ).stdout
        magic, width, height, depth, pixels = data.split(maxsplit=4)
        assert magic == b"P6" and depth == b"255"
        return int(width), int(height), pixels

    def screenshot(self, name):
        subprocess.run(
            ["grim", str(self.out / f"{name}.png")], env=self.env, check=False
        )


@dataclass
class Page:
    """A task page's element boxes, placed on the screen by its anchor square."""

    origin: tuple
    rects: dict

    def at(self, element, fx=0.5, fy=0.5):
        left, top, width, height = self.rects[element]
        return (
            round(self.origin[0] + left + width * fx),
            round(self.origin[1] + top + height * fy),
        )


def anchor(frame):
    """The top-left corner of the page's 6x6 magenta square."""
    width, _, pixels = frame
    run = b"\xff\x00\xff" * 6
    start = 0
    while (index := pixels.find(run, start)) >= 0:
        start = index + 1
        if index % 3:
            continue
        x, y = index // 3 % width, index // 3 // width
        rows = (pixels[(index + 3 * width * dy) : (index + 3 * width * dy) + 18] for dy in range(6))
        if all(row == run for row in rows) and pixels[index - 3 : index] != b"\xff\x00\xff":
            return x, y
    return None


def page(session, path):
    """Wait for the page's layout report, then find it on screen."""
    log = session.out / "layout.jsonl"

    def report():
        if not log.exists():
            return None
        entries = [json.loads(line) for line in log.read_text().splitlines()]
        return next((e for e in reversed(entries) if e["page"] == path), None)

    entry = wait_for(report, f"{path} did not report its layout")
    origin = wait_for(lambda: anchor(session.frame()), f"{path} is not visible on screen")
    (session.out / f"page{path.replace('/', '-')}.json").write_text(
        json.dumps({"origin": origin, **entry})
    )
    return Page(origin, entry["rects"])


class Driver:
    """A driver's MCP server, with every tool call traced to calls.jsonl."""

    def __init__(self, session, name):
        self.session = session
        command, extra = DRIVERS[name]
        env = session.env | extra
        if name == "cua":
            subprocess.run(
                ["/opt/cua/cua-driver", "telemetry", "disable"],
                env=env,
                capture_output=True,
                check=True,
            )
        self.process = subprocess.Popen(
            command,
            env=env,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=open(session.out / f"{name}.stderr", "w"),
            text=True,
            bufsize=1,
            start_new_session=True,
        )
        self.trace = open(session.out / "calls.jsonl", "w")
        self.errors = []
        self.next_id = 0
        self.request(
            "initialize",
            {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "waywire-acceptance", "version": "1"},
            },
        )
        self.send({"jsonrpc": "2.0", "method": "notifications/initialized"})

    def send(self, message):
        self.process.stdin.write(json.dumps(message) + "\n")
        self.process.stdin.flush()

    def request(self, method, params, timeout=120):
        self.next_id += 1
        self.send({"jsonrpc": "2.0", "id": self.next_id, "method": method, "params": params})
        deadline = time.monotonic() + timeout
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0 or not select.select([self.process.stdout], [], [], remaining)[0]:
                raise TimeoutError(f"{method} did not answer within {timeout} s")
            line = self.process.stdout.readline()
            if not line:
                raise RuntimeError(f"the driver exited during {method}")
            message = json.loads(line)
            if message.get("id") == self.next_id:
                return message

    def __call__(self, tool, **arguments):
        started = time.monotonic()
        message = self.request("tools/call", {"name": tool, "arguments": arguments})
        result = message.get("result") or {}
        text = "\n".join(
            block.get("text", "")
            for block in result.get("content", [])
            if block.get("type") == "text"
        )
        error = bool(message.get("error") or result.get("isError"))
        self.trace.write(
            json.dumps(
                {
                    "tool": tool,
                    "args": arguments,
                    "ms": round((time.monotonic() - started) * 1000, 1),
                    "error": error,
                    "text": (text or json.dumps(message.get("error")))[:4000],
                }
            )
            + "\n"
        )
        self.trace.flush()
        self.session.screenshot(f"call-{self.next_id - 1:02d}-{tool}")
        if error:
            self.errors.append(f"{tool}: {(text or str(message.get('error')))[:300]}")
        return text


def pause(seconds):
    """A wait the recorded run made (logged as an agent sleep)."""
    time.sleep(seconds)


# Start states, as setup_task.sh in the dogfood harness produced them.


def x11(cell):
    return cell.driver == "x11demo"


def chromium(session, cell, path):
    session.launch(
        "chromium",
        [
            "chromium",
            "--no-sandbox",
            "--test-type",
            f"--ozone-platform={'x11' if x11(cell) else 'wayland'}",
            f"--user-data-dir={SCRATCH}/chromium",
            "--no-first-run",
            "--no-default-browser-check",
            "--disable-features=Translate,MediaRouter",
            "--password-store=basic",
            "--disable-gpu",
            TASKS + path,
        ],
    )
    return page(session, path.split("?")[0])


def editor(session, cell, *files):
    session.launch(
        "editor",
        ["gnome-text-editor", "--standalone", "--ignore-session", *map(str, files)],
        XDG_STATE_HOME=f"{SCRATCH}/editor/state",
        XDG_DATA_HOME=f"{SCRATCH}/editor/data",
        GDK_BACKEND="x11" if x11(cell) else "wayland",
    )


def settle():
    # The dogfood setup waited 4 s after its last launch before handing over.
    time.sleep(4)


def form_start(session, cell, path="/form"):
    cell.page = chromium(session, cell, path)
    settle()


# T2 files in the dogfood run's order. The chooser's location bar completes
# from the directory, so each cell starts with the files earlier runs left.
T2_FILES = [f"t2-{d}-r{r}.txt" for d in ("cua", "cul-stock", "cul-patched", "x11demo") for r in (1, 2, 3)]


def editor_start(session, cell):
    name = f"t2-{cell.driver}-r{cell.run}.txt"
    for earlier in T2_FILES[: T2_FILES.index(name)]:
        (WORK / earlier).write_text("Shopping list\n- apples\n- bread\n")
    cell.file = WORK / name
    cell.file.write_text("Shopping list\n- apples\n- bread\n")
    editor(session, cell)
    session.wait_window("Text Editor|New Document|Untitled")
    settle()


def paste_start(session, cell):
    cell.token = "tk-" + os.urandom(4).hex()
    cell.page = chromium(session, cell, "/paste")
    script = f"echo 'Your access token is:'; echo '{cell.token}'; exec sh"
    if x11(cell):
        session.launch("token", ["xterm", "-T", "token", "-geometry", "100x36+128+100", "-e", "sh", "-c", script])
    else:
        session.launch("token", ["foot", "--title=token", "sh", "-c", script])
    session.wait_window("^token$")
    settle()


def upload_start(session, cell):
    upload = WORK / "upload"
    upload.mkdir()
    cell.file = upload / f"report-{cell.driver}-r{cell.run}.txt"
    cell.file.write_text(f"report for {cell.driver} run {cell.run}\n")
    cell.page = chromium(session, cell, "/upload")
    session.launch("thunar", ["thunar", str(upload)], GDK_BACKEND="x11" if x11(cell) else "wayland")
    session.wait_window("Thunar")
    settle()


def switch_start(session, cell):
    cell.value = "B-" + os.urandom(3).hex()
    cell.file = WORK / f"secret-{cell.driver}-r{cell.run}.txt"
    cell.file.write_text(f"The value for Window A is {cell.value}\n")
    cell.page = chromium(session, cell, "/notes")
    editor(session, cell, cell.file)
    session.wait_window("^secret-")
    settle()


SETUP = {
    "T1": form_start,
    "T2": editor_start,
    "T3": paste_start,
    "T4": upload_start,
    "T5": switch_start,
    "T6a": lambda session, cell: form_start(session, cell, "/form?interrupt=1"),
    "T6b": switch_start,
}


# Ground truth.


def submissions(cell, kind):
    log = cell.out / "submissions.jsonl"
    if not log.exists():
        return []
    entries = [json.loads(line) for line in log.read_text().splitlines()]
    return [{k: v for k, v in e.items() if k not in ("_t", "task")} for e in entries if e.get("task") == kind]


def expect_submission(kind, **expected):
    def check(session, cell):
        expected_now = {k: v(cell) if callable(v) else v for k, v in expected.items()}
        try:
            got = wait_for(lambda: submissions(cell, kind), f"no {kind} submission", 10)
        except RuntimeError as error:
            return False, str(error)
        time.sleep(1)
        got = submissions(cell, kind)
        if got != [expected_now]:
            return False, f"{kind} submissions {got}, expected exactly [{expected_now}]"
        return True, f"{kind} submission {got[0]}"

    return check


def expect_file(lines):
    def check(session, cell):
        expected = lines(cell)

        def actual():
            return [line for line in cell.file.read_text().splitlines() if line]

        try:
            wait_for(lambda: actual() == expected, "", 10)
        except RuntimeError:
            return False, f"{cell.file.name} has lines {actual()}, expected {expected}"
        return True, f"{cell.file.name} lines {expected}"

    return check


def expect_switch(session, cell):
    """The value setup wrote, submitted from Notes A, then focus back on that editor.

    The task requires switching by title; the replayed calls do (activate_window,
    bring_to_front and xdotool search --name), so the compositor must end with the
    editor showing this cell's file, and only it, activated.
    """
    ok, detail = expect_submission("switch", value=lambda c: c.value)(session, cell)
    if not ok:
        return ok, detail
    editor_title = f"{cell.file.name} (~/df/work) - Text Editor"
    focused = [w["title"] for w in session.toplevels() if w["activated"]]
    if focused != [editor_title]:
        return False, f"{detail}; focused toplevels {focused}, expected [{editor_title!r}]"
    return True, f"{detail}; compositor focus on {focused[0]!r}"


# Cells: each replays one passing dogfood run (waywire, df-calls.tsv rows).


@dataclass
class Cell:
    task: str
    driver: str
    run: int
    rows: str
    replay: object
    check: object
    setup: object = None
    known: dict = field(default_factory=dict)
    distros: tuple = ("bookworm", "trixie")
    out: Path = None
    page: Page = None
    file: Path = None
    token: str = ""
    value: str = ""
    before: dict = None
    notes: list = field(default_factory=list)
    failures: list = field(default_factory=list)

    @property
    def name(self):
        return f"{self.task}/{self.driver}/r{self.run}"


CELLS = []


def cell(task, driver, run, rows, check, setup=None, known=None, distros=("bookworm", "trixie")):
    """Register a cell. `known` maps a Debian codename to the cause of a known failure."""

    def register(replay):
        CELLS.append(Cell(task, driver, run, rows, replay, check, setup, known or {}, distros))
        return replay

    return register


def xy(point):
    return {"x": point[0], "y": point[1]}


def desktop(point):
    return {"scope": "desktop", "x": point[0], "y": point[1]}


def window_line(text, pattern):
    """pid and window_id of the first cua window line whose title matches."""
    for line in text.splitlines():
        found = re.search(r'window_id=(\d+) pid=(\d+) "([^"]*)"', line)
        if found and re.search(pattern, found[3]):
            return {"pid": int(found[2]), "window_id": int(found[1])}
    raise RuntimeError(f"no window matching {pattern!r} in the driver's list")


def element(app_state, predicate):
    """The index of the first AT-SPI element in a computer-use-linux tree that matches."""
    tree = json.loads(app_state)["accessibility_tree"]
    return next(node["index"] for node in tree if predicate(node))


def value_in(text):
    return re.search(r"B-[0-9a-f]{6}", text)[0]


def offset(before, after, x, y, width, height):
    """Where a strip of `before` reappears in `after`, as (dx, dy), or None."""
    stride = 3 * before[0]
    rows = [before[2][(y + r) * stride + 3 * x :][: 3 * width] for r in range(height)]
    for dy in range(-y, after[1] - y - height):
        for dx in range(-x, after[0] - x - width):
            start = (y + dy) * stride + 3 * (x + dx)
            if after[2][start : start + 3 * width] == rows[0] and all(
                after[2][start + r * stride : start + r * stride + 3 * width] == rows[r]
                for r in range(1, height)
            ):
                return dx, dy
    return None


def perturb(session, cell):
    """T6b's interloper: move the editor by its title bar after the agent has looked.

    The editor's title bar around the grab point must reappear at least 200 px
    to the right, or the cell fails: a move that did not happen tests nothing.
    """
    before = session.frame()
    session.run(sys.executable, str(HERE / "perturb_move.py"), "600,142", "1000,342")
    time.sleep(0.5)
    moved = offset(before, session.frame(), 500, 134, 200, 16)
    if not moved or moved[0] < 200:
        cell.failures.append(f"the interloper did not move the editor (title bar offset {moved})")
    else:
        cell.notes.append(f"interloper moved the editor by {moved}")


T1_FORM = dict(
    name="Katherine Johnson",
    email="kj@example.com",
    phone="555-0123",
    plan="Pro",
    terms="yes",
    notes="Dogfood run cul-patched r3",
)


@cell("T1", "cul-patched", 3, "88-92", expect_submission("form", **T1_FORM))
def _(d, session, c):
    d("screenshot")
    d("click", **xy(c.page.at("name")))
    d("type_text", text="Katherine Johnson\tkj@example.com\t555-0123\tP\t \tDogfood run cul-patched r3")
    d("click", **xy(c.page.at("submit")))
    d("screenshot")


@cell(
    "T1",
    "cua",
    6,
    "2117-2131",
    expect_submission(
        "form",
        name="Ada Lovelace",
        email="ada@example.com",
        phone="555-0100",
        plan="Enterprise",
        terms="yes",
        notes="Recorded run (cua, waywire)",
    ),
)
def _(d, session, c):
    # The recorded run paced its calls 700 ms apart for the viewer recording.
    for tool, arguments in [
        ("click", desktop(c.page.at("name"))),
        ("type_text", {"scope": "desktop", "text": "Ada Lovelace"}),
        ("click", desktop(c.page.at("email"))),
        ("type_text", {"scope": "desktop", "text": "ada@example.com"}),
        ("click", desktop(c.page.at("phone"))),
        ("type_text", {"scope": "desktop", "text": "555-0100"}),
        ("press_key", {"scope": "desktop", "key": "tab"}),
        ("type_text", {"scope": "desktop", "text": "E"}),
        ("press_key", {"scope": "desktop", "key": "tab"}),
        ("press_key", {"scope": "desktop", "key": "space"}),
        ("click", desktop(c.page.at("notes"))),
        ("type_text", {"scope": "desktop", "text": "Recorded run (cua, waywire)"}),
        ("click", desktop(c.page.at("submit"))),
    ]:
        d(tool, **arguments)
        pause(0.7)
    pause(1.5)
    d("get_desktop_state")


@cell(
    "T1",
    "x11demo",
    2,
    "107-110",
    expect_submission(
        "form",
        name="Grace Hopper",
        email="grace@example.com",
        phone="555-0199",
        plan="Enterprise",
        terms="yes",
        notes="Dogfood run x11demo r2",
    ),
)
def _(d, session, c):
    d("computer", action="screenshot")
    d("computer", action="left_click", coordinate=list(c.page.at("name")))
    d("computer", action="type", text="Grace Hopper\tgrace@example.com\t555-0199\tE\t \tDogfood run x11demo r2")
    d("computer", action="left_click", coordinate=list(c.page.at("submit")))


def t2_lines(cell):
    return ["Shopping list", "- apples", "- bread", f"- milk ({cell.driver} r{cell.run})"]


@cell("T2", "cul-patched", 1, "219-228", expect_file(t2_lines))
def _(d, session, c):
    windows = json.loads(d("list_windows"))["windows"]
    editor_id = next(w["window_id"] for w in windows if w["app_id"] == "org.gnome.TextEditor")
    d("press_key", window_id=editor_id, key="ctrl+o")
    pause(0.8)
    d("screenshot")
    d("press_key", key="ctrl+l")
    d("type_text", text=str(c.file))
    d("press_key", key="Return")
    pause(0.8)
    d("press_key", key="ctrl+End")
    d("type_text", text=f"\n- milk (cul-patched r{c.run})")
    d("press_key", key="ctrl+s")
    pause(0.5)
    d("screenshot")


# The Open popover's Browse item exists from gnome-text-editor 44; Debian 12 has 43.
@cell("T2", "cul-stock", 3, "209-218", expect_file(t2_lines), distros=("trixie",))
def _(d, session, c):
    # Stock has no window backend, so no chords: the Open popover, the chooser,
    # a click at the end of the last line, and Save from the main menu. On
    # 0548b28 the click after the chooser closed landed at the pointer's
    # previous position (fixed in 86cd002).
    d("screenshot")
    d("click", x=117, y=103)
    pause(0.6)
    d("click", x=411, y=156)
    pause(0.8)
    d("type_text", text=f"{c.file}\n")
    pause(0.8)
    d("click", x=400, y=188)
    d("type_text", text=f"\n- milk (cul-stock r{c.run})")
    d("screenshot")
    d("click", x=717, y=103)
    pause(0.6)
    d("click", x=632, y=324)
    pause(0.6)
    d("screenshot")


@cell(
    "T2",
    "x11demo",
    2,
    "264-272",
    expect_file(t2_lines),
    known={
        "trixie": "GTK 4.18's file chooser location bar inserts its inline completion while xdotool "
        "is still typing, so the path becomes t2-x11demo-r2.txtxt (dogfood T2/x11demo/r1, rows "
        "249-255). It fails the same way at 0548b28; the dogfood session typed more slowly (412 ms "
        "against 266 ms here). GTK 4.8 on Debian 12 does not race."
    },
)
def _(d, session, c):
    d("computer", action="screenshot")
    d("computer", action="key", text="ctrl+o")
    d("computer", action="key", text="ctrl+l")
    d("computer", action="type", text=str(c.file))
    d("computer", action="key", text="Return")
    d("computer", action="key", text="ctrl+End")
    d("computer", action="key", text="Return")
    d("computer", action="type", text=f"- milk (x11demo r{c.run})")
    d("computer", action="key", text="ctrl+s")


T3_CHECK = expect_submission("paste", token=lambda c: c.token)


@cell("T3", "cul-patched", 2, "326-332", T3_CHECK)
def _(d, session, c):
    d("screenshot")
    d("click", x=160, y=147, click_count=2)
    d("press_key", key="ctrl+shift+c", title="token")
    d("activate_window", title="Paste target - Chromium")
    d("click", **xy(c.page.at("token")))
    d("press_key", key="ctrl+v")
    d("click", **xy(c.page.at("check")))


@cell("T3", "cua", 2, "294-296", T3_CHECK)
def _(d, session, c):
    # The recorded run_steps batch, issued as its six steps.
    d("get_desktop_state")
    d("click", scope="desktop", x=160, y=147, count=2)
    d("hotkey", scope="desktop", keys=["ctrl", "shift", "c"])
    d("click", scope="desktop", x=600, y=50)
    d("click", **desktop(c.page.at("token")))
    d("hotkey", scope="desktop", keys=["ctrl", "v"])
    d("click", **desktop(c.page.at("check")))
    d("get_desktop_state")


@cell("T3", "x11demo", 2, "345-349", T3_CHECK)
def _(d, session, c):
    d("computer", action="screenshot")
    d("computer", action="triple_click", coordinate=[160, 121])
    d("computer", action="left_click", coordinate=[800, 48])
    d("computer", action="middle_click", coordinate=list(c.page.at("token", 0.15)))
    d("computer", action="left_click", coordinate=list(c.page.at("check")))


def t4_check(session, cell):
    return expect_submission(
        "upload",
        name=cell.file.name,
        size=str(len(cell.file.read_bytes())),
        head=cell.file.read_text(),
    )(session, cell)


@cell("T4", "cua", 3, "363-364", t4_check)
def _(d, session, c):
    d("get_desktop_state")
    d("drag", scope="desktop", from_x=366, from_y=270, to_x=116, to_y=300)


@cell("T4", "cul-patched", 2, "375-376", t4_check)
def _(d, session, c):
    d("screenshot")
    d("drag", start_x=366, start_y=270, end_x=116, end_y=300)


@cell("T4", "x11demo", 2, "388-394, shape of 1492-1499", t4_check)
def _(d, session, c):
    # waywire's run 388-394 released after one motion inside the drop zone. Thunar
    # and Chromium talk XDND directly inside Xwayland, and Chromium accepts only
    # from its second position message; with one motion the drop failed 6 of 6
    # times here at 0548b28 and at 86cd002. The Waymote run (also Xwayland) used
    # the shape agents learned: a second motion inside the zone before release.
    d("computer", action="screenshot")
    d("computer", action="mouse_move", coordinate=[366, 232])
    d("computer", action="left_mouse_down")
    d("computer", action="mouse_move", coordinate=[340, 240])
    d("computer", action="mouse_move", coordinate=[200, 280])
    d("computer", action="mouse_move", coordinate=[80, 300])
    d("computer", action="mouse_move", coordinate=[90, 305])
    d("computer", action="left_mouse_up")


@cell("T5", "cul-patched", 2, "527-534", expect_switch)
def _(d, session, c):
    windows = json.loads(d("list_windows"))["windows"]
    editor_title = next(w["title"] for w in windows if w["title"].startswith("secret-"))
    value = value_in(d("get_app_state", app_name_or_bundle_identifier="gnome-text-editor", include_screenshot=False, interactive_only=False))
    d("activate_window", title="Notes A - Chromium")
    d("click", **xy(c.page.at("a")))
    d("type_text", text=value)
    d("click", **xy(c.page.at("save")))
    d("activate_window", title=editor_title)
    d("focused_window")


@cell("T5", "cua", 2, "484-488", expect_switch)
def _(d, session, c):
    windows = d("list_windows")
    editor_window = window_line(windows, "^secret-")
    value = value_in(d("get_window_state", **editor_window))
    d("bring_to_front", **window_line(windows, "^Notes A - Chromium"))
    d("click", **desktop(c.page.at("a")))
    d("type_text", scope="desktop", text=value)
    d("click", **desktop(c.page.at("save")))
    d("bring_to_front", **editor_window)


@cell("T5", "x11demo", 2, "552-557", expect_switch)
def _(d, session, c):
    d("computer", action="screenshot")
    d("bash", command="xdotool search --name 'Notes A - Chromium' windowactivate --sync")
    d("computer", action="left_click", coordinate=list(c.page.at("a")))
    d("computer", action="type", text=c.value)
    d("computer", action="left_click", coordinate=list(c.page.at("save")))
    d("bash", command=f"xdotool search --name '^secret-x11demo-r{c.run}' windowactivate --sync; xdotool getactivewindow getwindowname")


def t6a_form(driver):
    return expect_submission(
        "form",
        name="Grace Hopper",
        email="gh@example.com",
        phone="555-0199",
        plan="Pro",
        terms="yes",
        notes=f"Dogfood run {driver} T6a r{3 if driver == 'cul-patched' else 2}",
    )


@cell("T6a", "cul-patched", 3, "613-624", t6a_form("cul-patched"))
def _(d, session, c):
    d("click", **xy(c.page.at("name")))
    d("type_text", text="Grace Hopper")
    d("click", **xy(c.page.at("email")))
    d("type_text", text="gh@example.com")
    d("click", **xy(c.page.at("phone")))
    pause(0.8)
    d("screenshot")
    d("press_key", key="Enter")
    d("click", **xy(c.page.at("phone")))
    d("type_text", text="555-0199\tP\t ")
    d("click", **xy(c.page.at("notes")))
    d("type_text", text="Dogfood run cul-patched T6a r3")
    d("click", **xy(c.page.at("submit")))


@cell("T6a", "x11demo", 2, "682-694", t6a_form("x11demo"))
def _(d, session, c):
    def click(point):
        d("computer", action="left_click", coordinate=list(point))

    click(c.page.at("name"))
    d("computer", action="type", text="Grace Hopper")
    click(c.page.at("email"))
    d("computer", action="type", text="gh@example.com")
    click(c.page.at("phone"))
    click((707, 218))
    d("computer", action="type", text="555-0199")
    click(c.page.at("terms"))
    click(c.page.at("notes"))
    d("computer", action="type", text="Dogfood run x11demo T6a r2")
    click(c.page.at("plan"))
    click((57, 502))
    click(c.page.at("submit"))


def t6b_lines(cell):
    return [f"The value for Window A is {cell.value}", "- verified"]


@cell("T6b", "cul-stock", 2, "761-763", expect_file(t6b_lines))
def _(d, session, c):
    perturb(session, c)
    state = d("get_app_state", app_name_or_bundle_identifier="gnome-text-editor", include_screenshot=False, interactive_only=False)
    text = element(state, lambda n: "The value for Window A" in str(n.get("value") or n.get("text") or ""))
    window = element(state, lambda n: "page.save" in n.get("actions", []))
    d("set_value", element_index=text, value=f"The value for Window A is {c.value}\n- verified\n")
    d("perform_action", element_index=window, action="page.save")


@cell("T6b", "cua", 2, "723-727", expect_file(t6b_lines))
def _(d, session, c):
    d("get_desktop_state")
    perturb(session, c)
    editor_window = window_line(d("list_windows"), "^secret-")
    state = d("get_window_state", **editor_window)
    token = re.search(r"element_token for row \[N\] = (s\d+):N", state)[1]
    row = re.search(r"\[(\d+)\][^\n]*The value for Window A", state)[1]
    d("set_value", **editor_window, element_token=f"{token}:{row}", value=f"The value for Window A is {c.value}\n- verified\n")
    d("hotkey", **editor_window, keys=["ctrl", "s"], delivery_mode="foreground")


@cell("T6b", "x11demo", 2, "776-780", expect_file(t6b_lines))
def _(d, session, c):
    perturb(session, c)
    d("bash", command=f"xdotool search --name '^secret-x11demo-r{c.run}' windowactivate --sync")
    d("computer", action="key", text="ctrl+End")
    d("computer", action="key", text="Return")
    d("computer", action="type", text="- verified")
    d("computer", action="key", text="ctrl+s")


# xdotool reaches X11 windows only; on Debian 12 waywire never draws them.
for _c in CELLS:
    if _c.driver == "x11demo" and _c.task != "T2":
        _c.known.setdefault("bookworm", OLD_XWAYLAND)

# Known failures on main. Each names its cause; see README.

PUNCTUATION = 'a?b(c)d!e:f"g{h}i<j>k_l+m~n'


@cell(
    "typeloss",
    "cul-patched",
    1,
    "2181-2193",
    # Blank fields are not part of a submission; terms stays unchecked.
    expect_submission("form", name=f"TL1 {PUNCTUATION}", plan="Basic"),
    setup=form_start,
    known=dict.fromkeys(
        ("bookworm", "trixie"),
        "Wayland virtual-keyboard text loses characters in Chromium (ROADMAP Pass 2 item 4); "
        "waywire's own NotifyKeyboardKeysym path types it intact",
    ),
)
def _(d, session, c):
    d("click", **xy(c.page.at("name")))
    d("type_text", text=f"TL1 {PUNCTUATION}\n")


def screencopy_check(session, cell):
    result = json.loads(session.run(sys.executable, str(HERE / "wl_probe.py"), "screencopy-destroy"))
    return bool(result.get("connection_survived")), json.dumps(result)


@cell(
    "screencopy-destroy",
    "probe",
    1,
    "-",
    screencopy_check,
    setup=lambda session, cell: None,
)
def _(d, session, c):
    pass


def chip_state(session):
    """The taskbar left of its status text and clock, and the toplevel count."""
    width, height, pixels = session.frame()
    bar = b"".join(
        pixels[3 * width * y : 3 * width * y + 3 * 1000] for y in range(height - 32, height)
    )
    return {"taskbar": hashlib.sha256(bar).hexdigest()[:16], "toplevels": len(session.toplevels())}


def chip_check(session, cell):
    after = chip_state(session)
    return after == cell.before, f"taskbar before {cell.before}, after {after}"


def chip_setup(session, cell):
    cell.before = chip_state(session)


@cell("chip-leak", "probe", 1, "-", chip_check, setup=chip_setup)
def _(d, session, c):
    # The dogfood repro, chipleak4.sh, verbatim: SIGKILL mapped foot clients.
    # Leaked chips have no title and the bar's colour, but they take space, so
    # the focused Terminal chip shrinks.
    subprocess.run(
        [
            "sh",
            "-c",
            "for i in $(seq 1 15); do foot -T kill9-$i sleep 60 >/dev/null 2>&1 & p=$!; "
            "sleep 1.0; kill -9 $p; sleep 0.3; done; sleep 1",
        ],
        env=session.env,
        check=True,
    )


def run_cell(cell, out):
    cell.out = out
    out.mkdir(parents=True)
    kill_everything_else()
    shutil.rmtree(SCRATCH, ignore_errors=True)
    for entry in HOME.iterdir():
        shutil.rmtree(entry) if entry.is_dir() and not entry.is_symlink() else entry.unlink()
    started = time.monotonic()
    replay_error = None
    session = None
    try:
        session = Session(out)
        (cell.setup or SETUP[cell.task])(session, cell)
        session.screenshot("start")
        driver = Driver(session, cell.driver) if cell.driver in DRIVERS else None
        try:
            cell.replay(driver, session, cell)
        except Exception as error:  # the verdict still comes from ground truth
            replay_error = f"{type(error).__name__}: {error}"
        ok, detail = cell.check(session, cell)
        if driver and driver.errors:
            detail += "; driver errors: " + " | ".join(driver.errors)
        session.screenshot("end")
    except Exception as error:
        ok, detail = False, f"harness: {type(error).__name__}: {error}"
        if session:
            session.screenshot("end")
    if replay_error:
        detail += f"; replay stopped: {replay_error}"
    if cell.failures:
        ok = False
        detail += "; " + "; ".join(cell.failures)
    if cell.notes:
        detail += "; " + "; ".join(cell.notes)
    known = cell.known.get(DISTRO, "")
    status = ("XPASS" if ok else "XFAIL") if known else ("PASS" if ok else "FAIL")
    return {
        "cell": cell.name,
        "status": status,
        "seconds": round(time.monotonic() - started, 1),
        "detail": detail,
        "known": known,
        "rows": cell.rows,
    }


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--out", type=Path)
    parser.add_argument("--list", action="store_true")
    parser.add_argument("cells", nargs="*")
    args = parser.parse_args()
    if args.list:
        for c in CELLS:
            notes = [f"{d} only" for d in c.distros if len(c.distros) == 1]
            notes += [f"known failure on {d}" for d in c.known]
            print(c.name + (f"  ({'; '.join(notes)})" if notes else ""))
        return 0
    chosen = [
        c
        for c in CELLS
        if DISTRO in c.distros and (not args.cells or any(c.name.startswith(p) for p in args.cells))
    ]
    if not chosen:
        raise SystemExit(f"no cell matches {args.cells}")
    results = []
    with open(args.out / "results.jsonl", "a") as log:
        for c in chosen:
            result = run_cell(c, args.out / c.name.replace("/", "-"))
            results.append(result)
            log.write(json.dumps(result) + "\n")
            log.flush()
            print(f"{result['status']:5} {result['seconds']:6.1f}s {c.name}: {result['detail']}", flush=True)
    kill_everything_else()
    failed = [r for r in results if r["status"] in ("FAIL", "XPASS")]
    print(f"{len(results) - len(failed)}/{len(results)} cells as expected", flush=True)
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
