#!/usr/bin/env python3
"""Drive the browser viewer through a live gateway and compositor.

The gateway starts the compositor with one fullscreen foot, whose program
records every byte foot sends it with mouse reporting in pixels. Headless
Chromium, driven by agent-browser, opens the viewer the way a person does and
checks that:

- the viewer shows a decoded, nonblank frame;
- a click on the screen reaches foot at the desktop pixel it was aimed at,
  through the viewer's scaling and letterboxing;
- text typed in the viewer reaches foot exactly;
- with Remote input switched off, clicks and keys reach nothing, and switching
  it back on gives the viewer control again;
- an unwatched desktop still updates without encoding browser video, and a
  reconnecting viewer receives the current static picture.

usage: viewer.py [BIN_DIR]   (default target/release; CI passes target/debug)
Requires the gateway's built web assets, agent-browser, Node.js and foot.
"""

import json
import math
import os
import shlex
import signal
import subprocess
import sys
import tempfile
import time
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
PORT = 18931
URL = f"http://127.0.0.1:{PORT}"
SESSION = f"viewer-e2e-{os.getpid()}"
DESKTOP = (1280, 720)
# Smaller than the desktop and a different shape, so the viewer scales the
# picture by 0.8 and letterboxes it 96 pixels from the top.
VIEWPORT = (1024, 768)
BANNER = "waywire viewer end-to-end test"
TYPED = "Hi, World! ~<|>_2"
# Mouse reports from xterm's SGR mode (1006), with pixel coordinates (1016).
PROGRAM = (
    f"for i in 1 2 3 4 5 6; do echo {shlex.quote(BANNER)}; done; "
    "stty raw -echo; printf '\\033[?1000h\\033[?1006h\\033[?1016h'; "
    'cat "$1" & '
    'exec cat > "$0"'
)
# The desktop pixel to click. At the viewport's 0.8 scale, the nearest page
# pixel lands a quarter pixel into it, so rounding cannot move the click.
CLICK = (901, 436)
# A click and keys sent while Remote input is off.
IGNORED_CLICK = (301, 237)
IGNORED_TEXT = "nope"
RESUMED_TEXT = "back"

# Counts pixels unlike the top-left one in what the viewer shows: the
# MediaStreamTrackGenerator <video> where Chromium has it, else the canvas.
NONBLANK = """(() => {
  const video = document.querySelector('#stage video');
  const source = video && video.videoWidth ? video : document.querySelector('#display');
  const width = video && video.videoWidth ? video.videoWidth : source.width;
  const height = video && video.videoWidth ? video.videoHeight : source.height;
  if (!width || !height) return 0;
  const canvas = document.createElement('canvas');
  canvas.width = width;
  canvas.height = height;
  const context = canvas.getContext('2d', { willReadFrequently: true });
  context.drawImage(source, 0, 0);
  const pixels = context.getImageData(0, 0, width, height).data;
  let count = 0;
  for (let i = 0; i < pixels.length; i += 4)
    if (Math.abs(pixels[i] - pixels[0]) + Math.abs(pixels[i + 1] - pixels[1])
        + Math.abs(pixels[i + 2] - pixels[2]) > 60) count++;
  return count;
})()"""


def browser(*args):
    return subprocess.run(
        ["agent-browser", "--session", SESSION, *args],
        check=True,
        text=True,
        stdout=subprocess.PIPE,
    ).stdout


def evaluate(expression):
    return json.loads(browser("--json", "eval", expression))["data"]["result"]


def wait_for(predicate, message, timeout=30):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if predicate():
            return
        time.sleep(0.1)
    raise AssertionError(message)


def http_ready():
    try:
        with urllib.request.urlopen(URL, timeout=1) as response:
            return response.status == 200
    except OSError:
        return False


def click(pixel):
    """Click the page point nearest a desktop pixel's center and return the
    desktop pixel under it, from the picture's letterboxed layout."""
    box = json.loads(
        evaluate(
            "JSON.stringify(document.querySelector('#display').getBoundingClientRect())"
        )
    )
    scale = min(box["width"] / DESKTOP[0], box["height"] / DESKTOP[1])
    left = box["x"] + (box["width"] - DESKTOP[0] * scale) / 2
    top = box["y"] + (box["height"] - DESKTOP[1] * scale) / 2
    x = round(left + (pixel[0] + 0.5) * scale)
    y = round(top + (pixel[1] + 0.5) * scale)
    browser("mouse", "move", str(x), str(y))
    browser("mouse", "down", "left")
    browser("mouse", "up", "left")
    return (math.floor((x - left) / scale), math.floor((y - top) / scale))


def type_text(text):
    subprocess.run(
        [
            "node",
            str(ROOT / "tools/e2e/type.mjs"),
            browser("get", "cdp-url").strip(),
            URL,
            text,
        ],
        check=True,
    )


def toggle_remote_input():
    browser("click", "#menu-key")
    browser("click", "label[for=control-toggle]")


def check(received, expected, message):
    """Wait for foot's program to have read exactly `expected`."""
    try:
        wait_for(lambda: received.read_bytes() == expected, message, timeout=10)
    except AssertionError:
        raise AssertionError(
            f"{message}: received {received.read_bytes()!r}, expected {expected!r}"
        ) from None
    print(f"ok: {message}", flush=True)


def press_report(pixel):
    # SGR pixel coordinates count from 1.
    x, y = pixel[0] + 1, pixel[1] + 1
    return f"\033[<0;{x};{y}M\033[<0;{x};{y}m".encode()


def run(bin_dir, directory):
    received = directory / "received"
    scene = directory / "scene"
    os.mkfifo(scene)
    env = os.environ.copy()
    for name in ("DISPLAY", "WAYLAND_DISPLAY", "WAYLAND_SOCKET"):
        env.pop(name, None)
    env["XDG_RUNTIME_DIR"] = str(directory)
    env["RUST_LOG"] = "info,waywire_latency=debug"
    command = [
        str(bin_dir / "waywire-gateway"),
        "--listen",
        f"127.0.0.1:{PORT}",
        "--public-url",
        URL,
        "--compositor",
        str(bin_dir / "waywire-compositor"),
        "--resolution",
        f"{DESKTOP[0]}x{DESKTOP[1]}",
        "--frame-rate",
        "30",
        "--",
        "foot",
        "--fullscreen",
        # Mouse reports count pixels from the grid; without padding, the
        # grid starts at the desktop's corner.
        "--override=pad=0x0",
        "sh",
        "-c",
        PROGRAM,
        str(received),
        str(scene),
    ]
    log = directory / "desktop.log"
    with log.open("w") as output:
        gateway = subprocess.Popen(
            command,
            env=env,
            stdout=output,
            stderr=subprocess.STDOUT,
            start_new_session=True,
        )
    try:
        wait_for(http_ready, "gateway did not serve the viewer")
        wait_for(received.exists, "foot did not start its program")
        browser("set", "viewport", str(VIEWPORT[0]), str(VIEWPORT[1]), "2")
        browser("open", URL)

        wait_for(
            lambda: evaluate(NONBLANK) > 500,
            "viewer did not show a decoded nonblank frame",
        )
        print("ok: viewer shows a decoded nonblank frame", flush=True)

        pixel = click(CLICK)
        expected = press_report(pixel)
        check(received, expected, f"click reached foot at desktop pixel {pixel}")

        type_text(TYPED)
        expected += TYPED.encode()
        check(received, expected, "typed text reached foot exactly")

        toggle_remote_input()
        wait_for(
            lambda: browser("get", "text", "#notice").strip() == "View only",
            "turning Remote input off did not show View only",
        )
        click(IGNORED_CLICK)
        type_text(IGNORED_TEXT)
        time.sleep(1)
        check(received, expected, "nothing reached foot with Remote input off")

        toggle_remote_input()
        pixel = click(CLICK)
        type_text(RESUMED_TEXT)
        expected += press_report(pixel) + RESUMED_TEXT.encode()
        check(received, expected, "turning Remote input on gave back control")

        browser("open", "about:blank")
        time.sleep(1)
        before = log.read_text()
        descriptor = os.open(scene, os.O_WRONLY | os.O_NONBLOCK)
        try:
            os.write(descriptor, b"\033[48;2;32;224;64m\033[2J")
        finally:
            os.close(descriptor)
        wait_for(
            lambda: 'stage="commit"' in log.read_text()[len(before) :],
            "foot did not commit its unwatched update",
        )
        time.sleep(1)
        assert log.read_text().count('stage="encoder_output"') == before.count(
            'stage="encoder_output"'
        ), "unwatched update still encoded browser video"
        print("ok: unwatched update does not encode browser video", flush=True)

        browser("open", URL)
        wait_for(
            lambda: evaluate("""(() => {
              const video = document.querySelector('#stage video');
              const source = video?.videoWidth ? video : document.querySelector('#display');
              if (!source) return false;
              const canvas = document.createElement('canvas');
              canvas.width = canvas.height = 1;
              const context = canvas.getContext('2d');
              try { context.drawImage(source, 0, 0, 1, 1); } catch { return false; }
              const [r, g, b] = context.getImageData(0, 0, 1, 1).data;
              return Math.abs(r - 32) < 32 && Math.abs(g - 224) < 32 && Math.abs(b - 64) < 32;
            })()"""),
            "reconnecting viewer did not show the current static green picture",
        )
        print("ok: reconnect shows the current static picture", flush=True)
    except BaseException:
        print(log.read_text(errors="replace")[-20000:], file=sys.stderr, flush=True)
        raise
    finally:
        if gateway.poll() is None:
            os.killpg(gateway.pid, signal.SIGTERM)
            try:
                gateway.wait(timeout=10)
            except subprocess.TimeoutExpired:
                os.killpg(gateway.pid, signal.SIGKILL)
                gateway.wait()


def main():
    bin_dir = Path(sys.argv[1] if len(sys.argv) > 1 else ROOT / "target/release")
    started = time.monotonic()
    with tempfile.TemporaryDirectory(prefix="waywire-viewer-e2e-") as tmp:
        os.chmod(tmp, 0o700)
        try:
            run(bin_dir.resolve(), Path(tmp))
        finally:
            subprocess.run(
                ["agent-browser", "--session", SESSION, "close"],
                check=False,
                stdout=subprocess.DEVNULL,
            )
    print(f"viewer end-to-end passed in {time.monotonic() - started:.1f} s")


if __name__ == "__main__":
    main()
