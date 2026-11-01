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
SCENARIOS = ("computer-use-linux", "cua-driver", "fedora-x11", "receipts-t6a")
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
