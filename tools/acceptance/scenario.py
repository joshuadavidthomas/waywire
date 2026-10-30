#!/usr/bin/env python3
"""Run a packaged-driver takeover in a disposable Docker container.

Requires Docker, gh, uv, agent-browser, ImageMagick, tesseract and host release binaries.
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
SCENARIOS = ("computer-use-linux", "cua-driver", "fedora-x11")


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
        if scenario == "cua-driver":
            archive = "cua-driver-rs-0.38.0-linux-x86_64-binary.tar.gz"
            run(
                "gh",
                "release",
                "download",
                "cua-driver-rs-v0.38.0",
                "-R",
                "trycua/cua",
                "-p",
                archive,
                "-p",
                "checksums.txt",
                "-D",
                str(temp),
            )
            line = next(
                line
                for line in (temp / "checksums.txt").read_text().splitlines()
                if line.endswith(archive)
            )
            run("sha256sum", "-c", "-", input=line + "\n", cwd=temp)
            run("tar", "-xzf", str(temp / archive), "-C", str(temp))
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
            elif scenario == "cua-driver":
                cua = subprocess.Popen(
                    [
                        "sudo",
                        "docker",
                        "exec",
                        NAME,
                        "python3",
                        "/repo/tools/acceptance/session_exec.py",
                        "/acceptance/cua-driver",
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
                docker("exec", NAME, "/acceptance/cua-driver", "telemetry", "disable")

                def action(method, payload):
                    result = docker(
                        "exec",
                        NAME,
                        "/acceptance/cua-driver",
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
