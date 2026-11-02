#!/usr/bin/env python3
"""Run a packaged-driver takeover in a disposable Docker container.

Requires Docker, uv, agent-browser, ImageMagick, tesseract and host release binaries.
The Docker daemon must support host networking. Run one scenario at a time.
"""

import json
import os
import re
import subprocess
import sys
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
ARTIFACTS = ROOT / ".amp/in/artifacts"
NAME = "waywire-acceptance"
BROWSER_SESSION = f"ww-{os.getpid()}"
PORT = "18925"
SCENARIOS = (
    "computer-use-linux",
    "cua-driver",
    "fedora-x11",
    "receipts-t6a",
    "window-query",
    "settle",
)
RECEIPTS = "/tmp/receipts.jsonl"


def run(*args, **kwargs):
    return subprocess.run(args, check=True, text=True, **kwargs)


def docker(*args, **kwargs):
    return run("sudo", "docker", *args, **kwargs)


def browser(*args):
    return run(
        "agent-browser", "--session", BROWSER_SESSION, *args, stdout=subprocess.PIPE
    ).stdout


def wait_for(predicate, message):
    deadline = time.monotonic() + 60
    while time.monotonic() < deadline:
        if predicate():
            return
        time.sleep(0.2)
    raise RuntimeError(message)


def exists_in_container(path):
    return (
        subprocess.run(
            ["sudo", "docker", "exec", NAME, "test", "-e", path],
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            check=False,
        ).returncode
        == 0
    )


def capture(scenario, phase):
    time.sleep(1)
    path = ARTIFACTS / f"{scenario}-{phase}.png"
    browser("screenshot", str(path))
    with tempfile.TemporaryDirectory(prefix="waywire-ocr-") as temp:
        cropped = Path(temp) / "terminal.png"
        run(
            "magick",
            str(path),
            "-crop",
            "1100x600+0+0",
            "-resize",
            "200%",
            str(cropped),
        )
        output = run(
            "tesseract",
            str(cropped),
            "stdout",
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
        ).stdout
    return re.sub(r"[^A-Z]", "", output.upper())


def session(*args, **kwargs):
    """Run a command in the container with the desktop session's environment."""
    return docker(
        "exec", NAME, "python3", "/repo/tools/acceptance/session_exec.py", *args, **kwargs
    )


def receipts():
    output = docker("exec", NAME, "cat", RECEIPTS, stdout=subprocess.PIPE).stdout
    return [json.loads(line) for line in output.splitlines() if line.strip()]


def settled():
    """Return the receipts once every signal for earlier input has arrived."""
    # GetFocus replies after the compositor has sent every earlier signal.
    session(
        "python3",
        "/repo/tools/receipts/receipts.py",
        "--focus",
        stdout=subprocess.DEVNULL,
    )
    time.sleep(0.5)
    return receipts()


def title(record, key="window"):
    return (record.get(key) or {}).get("title", "")


def named(records, prefix):
    """Ids of windows that any record has shown with a title starting with
    `prefix`. Records are snapshots, and Chromium can focus a field before
    it sets the page title, so a window is joined by id, not by title."""
    return {
        record["window"]["id"]
        for record in records
        if title(record).startswith(prefix) and record["window"].get("id")
    }


def keys(records):
    return [
        record
        for record in records
        if record["signal"] == "Receipt" and record["event"] == "key"
    ]


def t6a(variant, action):
    """Replay the dogfood run's T6a with cua-driver and check its receipts.

    The agent types a name and an email, tabs to Phone, looks at the screen
    for 1.5 s and types the phone number. Focusing Phone opens a dialog
    300 ms later, so the number never reaches the field.
    """
    start = len(receipts())
    chromium = subprocess.Popen(
        [
            "sudo",
            "docker",
            "exec",
            NAME,
            "python3",
            "/repo/tools/acceptance/session_exec.py",
            "chromium",
            "--no-sandbox",
            "--test-type",
            "--ozone-platform=wayland",
            f"--user-data-dir=/tmp/chromium-{variant}",
            "--no-first-run",
            "--no-default-browser-check",
            "--disable-gpu",
            "--disable-popup-blocking",
            f"file:///repo/tools/acceptance/t6a.html?interrupt={variant}",
        ],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    try:

        def form_ready():
            records = receipts()[start:]
            forms = named(records, "Registration form")
            return any(
                record["signal"] == "TextFieldChanged"
                and record["window"].get("id") in forms
                and record["text_field"] == "focused"
                for record in records
            )

        wait_for(form_ready, f"{variant}: the form never focused its Name field")
        action("type_text", {"scope": "desktop", "text": "Katherine Johnson"})
        action("press_key", {"scope": "desktop", "key": "tab"})
        action("type_text", {"scope": "desktop", "text": "kj@example.com"})
        action("press_key", {"scope": "desktop", "key": "tab"})
        time.sleep(1.5)
        mark = len(settled())
        action("type_text", {"scope": "desktop", "text": "555-0199"})
        records = settled()
    finally:
        subprocess.run(["sudo", "docker", "exec", NAME, "pkill", "chromium"], check=False)
        chromium.wait(timeout=20)
    before, after = records[start:mark], records[mark:]
    failures = []
    forms = named(before, "Registration form")
    form = next(
        (
            record["window"]
            for record in before
            if record["signal"] == "FocusChanged"
            and record["window"].get("id") in forms
        ),
        None,
    )
    if form is None:
        failures.append("no FocusChanged event named the form")
        form = {"id": None}
    typed = keys(before)
    phone = keys(after)
    if len(typed) < 2 * len("Katherine Johnson"):
        failures.append(f"{len(typed)} receipts for the name and email keys")
    if len(phone) < 2 * len("555-0199"):
        failures.append(f"{len(phone)} receipts for the phone keys")
    early = [record for record in typed if record["window"].get("id") != form["id"]]
    if early:
        failures.append(f"name or email keys not in the form: {early[:2]}")
    if not any(record.get("text_field") == "focused" for record in typed):
        failures.append("no name or email key reported a focused text field")
    if variant == "window":
        # Chromium maps the popup as about:blank and titles it afterwards,
        # so the events name it by window id and the receipts by its title.
        mapped = [
            record["window"]
            for record in before
            if record["signal"] == "WindowMapped"
            and record["window"].get("id") not in ("", form["id"])
        ]
        popup = mapped[-1] if mapped else {}
        focus = [
            record
            for record in before
            if record["signal"] == "FocusChanged"
            and popup.get("id")
            and record["window"].get("id") == popup["id"]
            and record["cause"] in ("new-window", "new-dialog", "activation")
        ]
        if not popup:
            failures.append("no WindowMapped event announced the popup")
        if not focus:
            failures.append("no FocusChanged event moved focus to the popup")
        stray = [
            record
            for record in phone
            if record["delivery"] != "window"
            or not popup.get("id")
            or record["window"].get("id") != popup["id"]
            or not title(record).startswith("Session expiring")
        ]
        landed = (
            f"in {title(phone[0]) if phone else '?'!r}, the window that took focus "
            f"({focus[0]['cause'] if focus else '?'})"
        )
    else:
        cleared = [
            record
            for record in before
            if record["signal"] == "TextFieldChanged"
            and record["window"].get("id") == form["id"]
            and record["text_field"] == "none"
        ]
        if not cleared:
            failures.append("no TextFieldChanged event reported the field losing focus")
        if any(
            record["signal"] == "WindowMapped" and record["window"].get("id") != form["id"]
            for record in before
        ):
            failures.append("a window mapped although alert() draws inside the page")
        stray = [
            record
            for record in phone
            if record["delivery"] != "window"
            or record["window"].get("id") != form["id"]
            or record.get("text_field") != "none"
        ]
        landed = f"in {form.get('title')!r} with no text field focused"
    if stray:
        failures.append(f"phone keys reported elsewhere: {stray[:2]}")
    if failures:
        return [f"{variant}: " + "; ".join(failures)]
    sources = sorted({record["source"] for record in phone})
    print(
        f"PASS {variant}: the {len(phone)} phone key events from {', '.join(sources)} "
        f"landed {landed}, not in the Phone field"
    )
    return []


# Window colours the scenario checks on screen with grim.
PAGE = (0x00, 0xCC, 0x66)
XTERM = (0x33, 0x66, 0xCC)


def windows():
    """The compositor's window list, as the reference consumer prints it."""
    output = session(
        "python3",
        "/repo/tools/receipts/receipts.py",
        "--windows",
        "--json",
        stdout=subprocess.PIPE,
    ).stdout
    return json.loads(output)


def window(listing, prefix):
    found = [w for w in listing["windows"] if w["title"].startswith(prefix)]
    if len(found) != 1:
        raise RuntimeError(f"expected one window titled {prefix!r}, got {listing}")
    return found[0]


def inside(point, rect):
    x, y, width, height = rect
    return x <= point[0] < x + width and y <= point[1] < y + height


def visible_point(listing, target):
    """A point in the target's content that no window above it covers,
    preferring its centre, as a driver picks where to click."""
    x, y, width, height = target["geometry"]
    above = [
        w["frame"]
        for w in listing["windows"]
        if w["stack"] < target["stack"] and not w["minimized"]
    ]
    candidates = [
        (x + width * fx / 8, y + height * fy / 8)
        for fx in range(1, 8)
        for fy in range(1, 8)
    ]
    centre = (x + width / 2, y + height / 2)
    candidates.sort(key=lambda p: (p[0] - centre[0]) ** 2 + (p[1] - centre[1]) ** 2)
    for point in candidates:
        if not any(inside(point, rect) for rect in above):
            return round(point[0]), round(point[1])
    raise RuntimeError(f"{target['title']!r} is entirely covered: {listing}")


def pixel(x, y):
    output = subprocess.run(
        [
            "sudo", "docker", "exec", NAME, "python3",
            "/repo/tools/acceptance/session_exec.py",
            "grim", "-t", "ppm", "-g", f"{x},{y} 1x1", "-",
        ],
        check=True,
        stdout=subprocess.PIPE,
    ).stdout
    return tuple(output[-3:])


def close_to(colour, expected):
    return all(abs(a - b) <= 8 for a, b in zip(colour, expected))


def window_query(pointer):
    """Move a window by its own request and by a driver's drag, then click
    each named window where its reported bounds and stacking say it shows,
    and check the click landed in it."""
    chromium = subprocess.Popen(
        [
            "sudo", "docker", "exec", NAME, "python3",
            "/repo/tools/acceptance/session_exec.py",
            "chromium", "--no-sandbox", "--test-type", "--ozone-platform=wayland",
            "--user-data-dir=/tmp/chromium-window-query", "--no-first-run",
            "--no-default-browser-check", "--disable-gpu", "--window-size=640,480",
            "--app=data:text/html,<title>Target page</title>"
            # rgb(), not #hex: '#' would start the data URL's fragment.
            "<body style='margin:0;background:rgb(%d,%d,%d)'>" % PAGE,
        ],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    session(
        "sh", "-c",
        f"xterm -title xterm-target -bg '#{bytes(XTERM).hex()}' -geometry 50x16+700+60"
        " >/dev/null 2>&1 &",
    )
    failures = []
    try:
        def ready():
            names = [w["title"] for w in windows()["windows"]]
            return any(n.startswith("Target page") for n in names) and any(
                n.startswith("xterm-target") for n in names
            )

        wait_for(ready, "Chromium and xterm never both appeared in the window list")
        time.sleep(1)
        mapped = {
            record["window"]["title"]: record["window"]["id"]
            for record in receipts()
            if record["signal"] in ("WindowMapped", "WindowChanged")
        }
        xid = session(
            "xdotool", "search", "--name", "^xterm-target$", stdout=subprocess.PIPE
        ).stdout.split()[0]

        def check(listing, step):
            ids = [w["id"] for w in listing["windows"]]
            if "" in ids or len(set(ids)) != len(ids):
                failures.append(f"{step}: ids are not unique and nonempty: {ids}")
            if [w["stack"] for w in listing["windows"]] != list(range(len(ids))):
                failures.append(f"{step}: stack positions are not 0..n topmost first")
            # Click topmost first, as the list orders them: a click raises
            # its window, which can cover one lower down entirely.
            targets = sorted(
                (("Target page", PAGE), ("xterm-target", XTERM)),
                key=lambda target: window(listing, target[0])["stack"],
            )
            for prefix, colour in targets:
                target = window(listing, prefix)
                announced = [i for t, i in mapped.items() if t.startswith(prefix)]
                if target["id"] not in announced:
                    failures.append(
                        f"{step}: {prefix!r} is listed as {target['id']!r},"
                        f" but WindowMapped named it {announced}"
                    )
                point = visible_point(listing, target)
                seen = pixel(*point)
                if not close_to(seen, colour):
                    failures.append(
                        f"{step}: the screen at {point}, inside {prefix!r}'s reported"
                        f" bounds {target['geometry']}, shows {seen}, not {colour}"
                    )
                start = len(receipts())
                pointer("click", *point)
                session(
                    "python3", "/repo/tools/receipts/receipts.py", "--focus",
                    stdout=subprocess.DEVNULL,
                )
                time.sleep(0.3)
                presses = [
                    r
                    for r in receipts()[start:]
                    if r["signal"] == "Receipt"
                    and r["event"] == "button"
                    and r["pressed"]
                ]
                landed = [r["window"].get("id") for r in presses]
                if landed != [target["id"]]:
                    failures.append(
                        f"{step}: a click at {point} for {prefix!r} ({target['id']})"
                        f" landed in {[r['window'].get('title') for r in presses]} {landed}"
                    )
                after = window(windows(), prefix)
                if after["stack"] != 0 or not after["focused"]:
                    failures.append(f"{step}: {prefix!r} is not on top and focused after its click")
                listing = windows()
            x11 = window(listing, "xterm-target")["geometry"]
            shell = session(
                "xdotool", "getwindowgeometry", "--shell", xid, stdout=subprocess.PIPE
            ).stdout
            fields = dict(line.split("=", 1) for line in shell.split())
            own = [int(fields[k]) for k in ("X", "Y", "WIDTH", "HEIGHT")]
            if own != list(x11):
                failures.append(f"{step}: X11 says the xterm is at {own}, the list {x11}")
            return listing

        # The xterm moves itself, to the far corner.
        listing = windows()
        xterm = window(listing, "xterm-target")
        area = listing["work_area"]
        x = area[0] + area[2] - xterm["geometry"][2] - 20
        y = area[1] + area[3] - xterm["geometry"][3] - 20
        session("xdotool", "windowmove", xid, str(x), str(y))
        time.sleep(0.5)
        listing = check(windows(), "after the xterm's own move")
        if window(listing, "xterm-target")["geometry"][:2] != [x, y]:
            failures.append(f"the xterm asked for ({x}, {y}): {window(listing, 'xterm-target')}")

        # A driver drags the xterm by its titlebar until it covers the
        # page's centre, which leaves only the page's edges to click.
        page = window(listing, "Target page")["geometry"]
        xterm = window(listing, "xterm-target")
        fx, fy, fw, _ = xterm["frame"]
        gx, gy, gw, gh = xterm["geometry"]
        grab = (fx + fw / 2, fy + (gy - fy) / 2)
        dx = page[0] + page[2] / 2 - (gx + gw / 2)
        dy = page[1] + page[3] / 2 - (gy + gh / 2)
        pointer("drag", *grab, grab[0] + dx, grab[1] + dy)
        time.sleep(0.5)
        listing = windows()
        moved = window(listing, "xterm-target")["geometry"]
        if abs(moved[0] - (gx + dx)) > 2 or abs(moved[1] - (gy + dy)) > 2:
            failures.append(f"the drag left the xterm at {moved}, not {(gx + dx, gy + dy)}")
        check(listing, "after the driver's drag")
    except Exception as error:
        # A wrong list can make a later step impossible, such as a drag
        # from a point off screen; report it with what failed before it.
        failures.append(f"stopped: {error!r}"[:600])
    finally:
        subprocess.run(["sudo", "docker", "exec", NAME, "pkill", "chromium"], check=False)
        subprocess.run(["sudo", "docker", "exec", NAME, "pkill", "xterm"], check=False)
        chromium.wait(timeout=20)
    if failures:
        raise RuntimeError("window-query failed: " + " | ".join(failures))
    print("PASS window-query: every click at a reported window's bounds landed in that window")


# The settle page: its colour before a click, the colour it ends on, and how
# long after the click it keeps repainting before it does.
GREY = (40, 40, 40)
FINAL = (0, 204, 102)
REPAINT_MS = 1200


def screen(path):
    """A grim PPM capture saved in the container: (width, rows of pixels)."""
    data = subprocess.run(
        ["sudo", "docker", "exec", NAME, "cat", path], check=True, stdout=subprocess.PIPE
    ).stdout
    fields = data.split(maxsplit=4)
    width, height = int(fields[1]), int(fields[2])
    pixels = fields[4][: width * height * 3]
    return width, [pixels[row * width * 3 : (row + 1) * width * 3] for row in range(height)]


def region(capture, rect):
    width, rows = capture
    x, y, w, h = rect
    return [row[x * 3 : (x + w) * 3] for row in rows[y : y + h]]


def at(capture, point):
    width, rows = capture
    x, y = point
    return tuple(rows[y][x * 3 : x * 3 + 3])


def overlap(a, b):
    return a[0] < b[0] + b[2] and b[0] < a[0] + a[2] and a[1] < b[1] + b[3] and b[1] < a[1] + a[3]


def settle_scenario(pointer):
    """Click a page that repaints for 1.2 s, wait on WaitSettled, and check
    that the screenshot taken right after it returns shows the page's final
    state and that nothing changed afterwards. Then start an endless
    animation and check that the page's wait times out while a quiet window
    elsewhere settles."""
    chromium = subprocess.Popen(
        [
            "sudo", "docker", "exec", NAME, "python3",
            "/repo/tools/acceptance/session_exec.py",
            "chromium", "--no-sandbox", "--test-type", "--ozone-platform=wayland",
            "--user-data-dir=/tmp/chromium-settle", "--no-first-run",
            "--no-default-browser-check", "--disable-gpu", "--window-size=640,480",
            "--app=file:///repo/tools/acceptance/settle.html",
        ],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    session(
        "sh", "-c",
        f"xterm -title xterm-quiet -bg '#{bytes(XTERM).hex()}' -geometry 30x8+0+0"
        " >/dev/null 2>&1 &",
    )
    failures = []
    try:
        def ready():
            names = [w["title"] for w in windows()["windows"]]
            return any(n.startswith("Settle target") for n in names) and any(
                n.startswith("xterm-quiet") for n in names
            )

        wait_for(ready, "Chromium and xterm never both appeared in the window list")
        listing = windows()
        # Keep the xterm clear of the page, so the page's repaints are
        # outside the xterm's part of the screen.
        area = listing["work_area"]
        x = area[0] + area[2] - window(listing, "xterm-quiet")["geometry"][2] - 20
        xid = session(
            "xdotool", "search", "--name", "^xterm-quiet$", stdout=subprocess.PIPE
        ).stdout.split()[0]
        session("xdotool", "windowmove", xid, str(x), str(area[1] + 20))

        def painted():
            listing = windows()
            point = visible_point(listing, window(listing, "Settle target"))
            return close_to(pixel(*point), GREY) and not overlap(
                window(listing, "Settle target")["frame"],
                window(listing, "xterm-quiet")["frame"],
            )

        wait_for(painted, "the page never painted its first colour clear of the xterm")
        listing = windows()
        page = window(listing, "Settle target")
        xterm = window(listing, "xterm-quiet")
        point = visible_point(listing, page)

        # The click starts 1.2 s of repaints that end in green.
        pointer("click", *point)
        answer = json.loads(pointer("settle", 300, 5000, "-", "/tmp/settle-after.ppm"))
        time.sleep(1.5)
        session("grim", "-t", "ppm", "/tmp/settle-later.ppm")
        after, later = screen("/tmp/settle-after.ppm"), screen("/tmp/settle-later.ppm")
        for name in ("after", "later"):
            ppm = ARTIFACTS / f"settle-{name}.ppm"
            docker("cp", f"{NAME}:/tmp/settle-{name}.ppm", str(ppm), stdout=subprocess.DEVNULL)
            run("magick", str(ppm), str(ppm.with_suffix(".png")))
            ppm.unlink()
        waited = (answer["returned_usec"] - answer["clicked_usec"]) / 1000
        last = answer["damage_usec"] and (answer["damage_usec"] - answer["input_usec"]) / 1000
        print(
            f"settle: {answer['result']} {waited:.0f} ms after the click;"
            f" the compositor saw {answer['frames']} changed frames"
            + (f", the last {last:.0f} ms after the input" if last else "")
        )
        if answer["result"] != "settled":
            failures.append(f"the page's wait ended with {answer['result']}: {answer}")
        if not close_to(at(after, point), FINAL):
            failures.append(
                f"the screenshot right after the wait shows {at(after, point)} at {point},"
                f" not the final {FINAL}: it settled before the page stopped repainting"
            )
        if region(after, page["geometry"]) != region(later, page["geometry"]):
            failures.append("the page changed after the wait said it had settled")
        if waited < REPAINT_MS:
            failures.append(
                f"the wait returned {waited:.0f} ms after the click, before the page's"
                f" {REPAINT_MS} ms of repaints ended"
            )

        # The second click starts an endless animation: the page never holds
        # still, but the xterm does.
        pointer("click", *point)
        busy = json.loads(pointer("settle", 300, 1500, "-", "/tmp/settle-busy.ppm"))
        spent = (busy["returned_usec"] - busy["called_usec"]) / 1000
        print(f"settle while animating: {busy['result']} after {spent:.0f} ms")
        if busy["result"] != "timeout":
            failures.append(f"an endless animation ended with {busy['result']}, not timeout")
        if not 1500 <= spent < 3000:
            failures.append(f"a 1500 ms timeout returned after {spent:.0f} ms")
        quiet = json.loads(pointer("settle", 300, 3000, xterm["id"], "/tmp/settle-quiet.ppm"))
        print(
            f"settle on the xterm while the page animates: {quiet['result']},"
            f" {quiet['frames']} changed frames"
        )
        if quiet["result"] != "settled" or quiet["frames"]:
            failures.append(f"the still xterm did not settle while the page animated: {quiet}")
    except Exception as error:
        failures.append(f"stopped: {error!r}"[:600])
    finally:
        subprocess.run(["sudo", "docker", "exec", NAME, "pkill", "chromium"], check=False)
        subprocess.run(["sudo", "docker", "exec", NAME, "pkill", "xterm"], check=False)
        chromium.wait(timeout=20)
    if failures:
        raise RuntimeError("settle failed: " + " | ".join(failures))
    print("PASS settle: the screenshot after WaitSettled showed the page's final state")


def main(scenario):
    if scenario not in SCENARIOS:
        raise SystemExit(f"Choose one of: {', '.join(SCENARIOS)}")
    ARTIFACTS.mkdir(parents=True, exist_ok=True)
    fedora = scenario == "fedora-x11"
    image = "waywire-acceptance-" + ("fedora" if fedora else "debian")
    docker(
        "build",
        "--network",
        "host",
        "-f",
        str(
            ROOT
            / "tools/acceptance"
            / ("Dockerfile.fedora" if fedora else "Dockerfile.debian")
        ),
        "-t",
        image,
        str(ROOT / "tools/acceptance"),
    )

    with tempfile.TemporaryDirectory(prefix="waywire-acceptance-") as temp:
        temp = Path(temp)
        if fedora:
            run(
                "git",
                "clone",
                "--quiet",
                "https://github.com/anthropics/anthropic-quickstarts.git",
                str(temp / "anthropic"),
            )
            run(
                "git",
                "-C",
                str(temp / "anthropic"),
                "checkout",
                "--quiet",
                "1a18c340ca42b9137d991637afe867bdcf70a9ce",
            )
            run(
                "uv",
                "pip",
                "install",
                "--target",
                str(temp / "deps"),
                "--python-version",
                "3.13",
                "--python-platform",
                "x86_64-manylinux2014",
                "anthropic==2.3.0",
            )
        mounts = ["-v", f"{ROOT}:/repo:ro", "-v", f"{temp}:/acceptance:ro"]
        gateway = monitor = driver = cua = None
        created = False
        log = (ARTIFACTS / f"{scenario}-bus.txt").open("w")
        try:
            docker(
                "run",
                "--rm",
                "-d",
                "--name",
                NAME,
                "--network",
                "host",
                *mounts,
                image,
                "sleep",
                "infinity",
                stdout=subprocess.DEVNULL,
            )
            created = True
            docker(
                "exec",
                NAME,
                "sh",
                "-c",
                "mkdir -p /tmp/runtime /tmp/.X11-unix; chmod 700 /tmp/runtime; chmod 1777 /tmp/.X11-unix",
            )
            program = (
                "exec xterm -title acceptance"
                if fedora
                else "exec foot --title=acceptance"
            )
            gateway = subprocess.Popen(
                [
                    "sudo",
                    "docker",
                    "exec",
                    "-e",
                    "XDG_RUNTIME_DIR=/tmp/runtime",
                    NAME,
                    "/repo/target/release/waywire-gateway",
                    "--listen",
                    f"127.0.0.1:{PORT}",
                    "--public-url",
                    f"http://127.0.0.1:{PORT}",
                    "--resolution",
                    "1280x720",
                    "--compositor",
                    "/repo/target/release/waywire-compositor",
                    "--",
                    "/bin/sh",
                    "-c",
                    f"umask 077; env -0 > /tmp/session.env; {program}",
                ]
            )
            wait_for(
                lambda: exists_in_container("/tmp/session.env"), "session did not start"
            )
            monitor = subprocess.Popen(
                [
                    "sudo",
                    "docker",
                    "exec",
                    NAME,
                    "python3",
                    "/repo/tools/acceptance/session_exec.py",
                    "sh",
                    "-c",
                    'exec dbus-monitor --address "$DBUS_SESSION_BUS_ADDRESS"',
                ],
                stdout=log,
                stderr=subprocess.STDOUT,
            )
            if scenario in ("window-query", "settle"):
                # The pointer driver runs under the reference consumer, which
                # records its receipts and every window event.
                driver = subprocess.Popen(
                    [
                        "sudo", "docker", "exec", "-i", NAME, "python3",
                        "/repo/tools/acceptance/session_exec.py",
                        "python3", "/repo/tools/receipts/receipts.py", "--json",
                        "--output", RECEIPTS, "--",
                        "python3", "/repo/tools/acceptance/portal_pointer.py",
                    ],
                    stdin=subprocess.PIPE,
                    stdout=subprocess.PIPE,
                    text=True,
                    bufsize=1,
                )
                if driver.stdout.readline().strip() != "ok":
                    raise RuntimeError("the portal pointer did not start")

                def pointer(*command):
                    driver.stdin.write(" ".join(str(part) for part in command) + "\n")
                    driver.stdin.flush()
                    status, _, result = driver.stdout.readline().strip().partition(" ")
                    if status != "ok":
                        raise RuntimeError(f"portal pointer {command} failed")
                    return result

                try:
                    if scenario == "settle":
                        settle_scenario(pointer)
                    else:
                        window_query(pointer)
                finally:
                    (ARTIFACTS / f"{scenario}.jsonl").write_text(
                        "".join(json.dumps(record) + "\n" for record in receipts())
                    )
                return
            if scenario == "computer-use-linux":
                driver = subprocess.Popen(
                    [
                        "sudo",
                        "docker",
                        "exec",
                        "-i",
                        NAME,
                        "python3",
                        "/repo/tools/acceptance/portal_mcp.py",
                    ],
                    stdin=subprocess.PIPE,
                    stdout=subprocess.PIPE,
                    text=True,
                    bufsize=1,
                )
                ready = json.loads(driver.stdout.readline())
                if not ready.get("ready"):
                    raise RuntimeError(f"MCP initialization failed: {ready}")

                def action(name, **kwargs):
                    driver.stdin.write(
                        json.dumps({"name": name, "arguments": kwargs}) + "\n"
                    )
                    driver.stdin.flush()
                    result = json.loads(driver.stdout.readline())
                    if not result.get("ok"):
                        raise RuntimeError(f"MCP {name}: {result}")

                action("screenshot", full_screen=True)
                action("click", x=100, y=100)

                def command(marker):
                    action("type_text", text=f"echo {marker}")
                    action("press_key", key="Return")
            elif scenario in ("cua-driver", "receipts-t6a"):
                # The compositor sends receipts only to the driver's own
                # process tree, so the reference consumer starts the driver.
                watch = (
                    [
                        "python3",
                        "/repo/tools/receipts/receipts.py",
                        "--json",
                        "--output",
                        RECEIPTS,
                        "--",
                    ]
                    if scenario == "receipts-t6a"
                    else []
                )
                cua = subprocess.Popen(
                    [
                        "sudo",
                        "docker",
                        "exec",
                        NAME,
                        "python3",
                        "/repo/tools/acceptance/session_exec.py",
                        *watch,
                        "/opt/cua/cua-driver",
                        "serve",
                        "--no-overlay",
                        "--socket",
                        "/tmp/cua.sock",
                    ]
                )
                wait_for(
                    lambda: exists_in_container("/tmp/cua.sock"),
                    "cua server did not start",
                )
                docker("exec", NAME, "/opt/cua/cua-driver", "telemetry", "disable")

                def action(method, payload):
                    result = docker(
                        "exec",
                        NAME,
                        "/opt/cua/cua-driver",
                        "call",
                        "--socket",
                        "/tmp/cua.sock",
                        method,
                        json.dumps(payload),
                        stdout=subprocess.PIPE,
                    ).stdout
                    response = json.loads(result)
                    if response.get("error") or response.get("isError"):
                        raise RuntimeError(f"cua {method}: {response}")

                action(
                    "get_desktop_state", {"screenshot_out_file": "/tmp/cua-screen.png"}
                )
                action("click", {"scope": "desktop", "x": 100, "y": 100})

                def command(marker):
                    action("type_text", {"scope": "desktop", "text": f"echo {marker}"})
                    action("press_key", {"scope": "desktop", "key": "enter"})
            else:

                def action(kind, value):
                    docker(
                        "exec",
                        "-e",
                        "PYTHONPATH=/acceptance/deps:/acceptance/anthropic/computer-use-demo",
                        NAME,
                        "python3",
                        "/repo/tools/acceptance/anthropic_action.py",
                        kind,
                        value,
                    )

                def command(marker):
                    action("type", f"echo {marker}")
                    action("key", "Return")

            if scenario == "receipts-t6a":
                try:
                    failures = t6a("alert", action) + t6a("window", action)
                    if failures:
                        raise RuntimeError("receipts-t6a failed: " + " | ".join(failures))
                finally:
                    (ARTIFACTS / f"{scenario}.jsonl").write_text(
                        "".join(json.dumps(record) + "\n" for record in receipts())
                    )
                return
            browser("open", f"http://127.0.0.1:{PORT}/")
            browser("set", "viewport", "1280", "720", "2")
            command("AGENT_BEFORE")
            before = capture(scenario, "driving")
            browser("mouse", "move", "100", "100")
            browser("mouse", "down", "left")
            browser("mouse", "up", "left")
            command("AGENT_DURING")
            during = capture(scenario, "takeover")
            snapshot = browser("snapshot").lower()
            notice = "you have control" in snapshot and "agent paused" in snapshot
            browser("click", "button[aria-label='Menu']")
            browser("click", "label[for='control-toggle']")
            command("AGENT_AFTER")
            after = capture(scenario, "resumed")
            if (
                before.count("AGENTBEFORE") < 2
                or "AGENTDURING" in during
                or after.count("AGENTAFTER") < 2
                or not notice
            ):
                raise RuntimeError(
                    f"acceptance failed: paused notice={notice}; OCR={before!r}, {during!r}, {after!r}"
                )
            print(
                "PASS: BEFORE and AFTER visible, DURING absent, paused notice visible"
            )
        finally:
            subprocess.run(
                ["agent-browser", "--session", BROWSER_SESSION, "close"],
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
                check=False,
            )
            if driver and driver.stdin:
                driver.stdin.close()
            if created:
                subprocess.run(
                    ["sudo", "docker", "rm", "-f", NAME],
                    stdout=subprocess.DEVNULL,
                    stderr=subprocess.DEVNULL,
                    check=False,
                )
            for process in (driver, cua, monitor, gateway):
                if process:
                    try:
                        process.wait(timeout=10)
                    except subprocess.TimeoutExpired:
                        process.terminate()
                        process.wait(timeout=5)
            log.close()


if __name__ == "__main__":
    if len(sys.argv) != 2:
        raise SystemExit(f"Usage: {sys.argv[0]} <{'|'.join(SCENARIOS)}>")
    main(sys.argv[1])
