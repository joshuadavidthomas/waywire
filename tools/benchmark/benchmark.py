#!/usr/bin/env python3
"""Compare the two desktops in the same orb.

Both use xterm at 1280x720, 30 fps and a 12000 kbps encoder target. The
viewer is Chromium controlled by agent-browser; a decoded WebCodecs frame,
not a listening HTTP port, marks the end of launch. Desktop process CPU and
RSS will be sampled separately from the viewer. Typing and scrolling use the
same application and workload on each desktop; bitrate counts viewer video
WebSocket payloads. Input latency is measured from a browser key event to a
changed decoded pixel, not from server-side timestamps. Agent checks use
XTest through xdotool and the upstream Anthropic tool with grim capture.

Regression limits: waywire may exceed Waymote by 20% plus 2 CPU percentage
points, 15% plus 10 MiB RSS, 25% plus 20 kbps stream traffic, 25% plus
200 ms launch time, or 25% plus 10 ms latency. Each agent must pass if the
corresponding Waymote agent passes. Missing comparisons fail the check.

Slow link: with each encoder, waywire streams a full-screen animation in
Chromium while the viewer's link drops to 500 kbps for 45 seconds, then clears.
On the slow link the viewer must lower the encoded scale and the bitrate target,
not only the frame rate. Over the last 15 seconds on the slow link it must
still draw at least 4 frames a second, and the median decoded frame must be at
most 5 seconds old, measured from capture on the host's monotonic clock. Once
the link clears, quality must return to the full scale, bitrate and frame rate
within 150 seconds. Both encoders drew about 5 frames a second about 3.5
seconds old: the ladder stops at half the bitrate ceiling, far above 500 kbps.

Requires release binaries (just build), agent-browser, xterm, ffmpeg, Xwayland,
xdotool, grim, wlr-randr, Debian Chromium, git, uv and the Amp Waymote installation.
The pinned Anthropic demo and Python dependency are downloaded to a temporary
directory on each run. Run one benchmark at a time.
"""

import argparse
import json
import math
import os
import shlex
import signal
import statistics
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
from pathlib import Path

from link import Link

ROOT = Path(__file__).resolve().parents[2]
WAYMOTE = Path("/opt/amp-wayland")
PORT = 18925
SESSION = f"benchmark-{os.getpid()}"
URL = f"http://127.0.0.1:{PORT}"
SECONDS = 8
SLOW_LINK_KBPS = 500
SLOW_LINK_SECONDS = 45
SLOW_LINK_MIN_FPS = 4
SLOW_LINK_MAX_AGE_MS = 5000
RECOVERY_SECONDS = 150
ENCODERS = ("ffmpeg", "openh264")
MOTION_PAGE = (
    "<!doctype html><title>Benchmark motion</title><style>"
    "html,body{margin:0;overflow:hidden;font:15px sans-serif}"
    "@keyframes m{from{transform:translateY(0)}to{transform:translateY(-3000px)}}"
    "div{animation:m 20s linear infinite}</style><div>"
    + "".join(
        f"<p>Motion {i}: the quick brown fox jumps over the lazy dog.</p>"
        for i in range(400)
    )
    + "</div>"
)


def evaluate(expression):
    return json.loads(browser("--json", "eval", expression))["data"]["result"]


def process_snapshot(roots, sessions=()):
    processes = {}
    for path in Path("/proc").iterdir():
        if not path.name.isdigit():
            continue
        try:
            stat = (path / "stat").read_text().rsplit(")", 1)[1].split()
            pid = int(path.name)
            processes[pid] = (
                int(stat[1]),
                int(stat[11]) + int(stat[12]),
                int(stat[19]),
                int(stat[21]),
                int(stat[3]),
            )
        except (FileNotFoundError, ProcessLookupError, PermissionError, ValueError):
            continue
    included = set(roots) | {
        pid for pid, values in processes.items() if values[4] in sessions
    }
    while True:
        more = {
            pid for pid, values in processes.items() if values[0] in included
        } - included
        if not more:
            break
        included.update(more)
    return {
        (pid, processes[pid][2]): (processes[pid][1], processes[pid][3])
        for pid in included
        if pid in processes
    }


def measure(roots, action=None, env=None, sessions=()):
    before = evaluate(
        "({time: performance.now(), sockets: window.benchmarkSockets, frames: window.benchmarkFrames})"
    )
    previous = process_snapshot(roots, sessions)
    ticks = 0
    rss = []
    started = time.monotonic()
    task = (
        subprocess.Popen(
            action, env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL
        )
        if action
        else None
    )
    while time.monotonic() - started < SECONDS:
        time.sleep(0.2)
        current = process_snapshot(roots, sessions)
        ticks += sum(
            max(0, values[0] - previous.get(key, (values[0], 0))[0])
            for key, values in current.items()
        )
        rss.append(sum(values[1] for values in current.values()))
        previous = current
    elapsed = time.monotonic() - started
    if task:
        task.wait(timeout=10)
        if task.returncode:
            raise RuntimeError(f"workload failed: {action}")
    after = evaluate(
        "({time: performance.now(), sockets: window.benchmarkSockets, frames: window.benchmarkFrames})"
    )
    paths = set(before["sockets"]) | set(after["sockets"])
    rates = {
        path: round(
            (after["sockets"].get(path, 0) - before["sockets"].get(path, 0))
            * 8
            / (after["time"] - before["time"]),
            1,
        )
        for path in paths
    }
    result = {
        "cpu_percent": round(100 * ticks / os.sysconf("SC_CLK_TCK") / elapsed, 1),
        "rss_mib": round(sum(rss) / len(rss) * os.sysconf("SC_PAGE_SIZE") / 1048576, 1),
        "kbps_by_socket": rates,
        "frames": after["frames"] - before["frames"],
    }
    if action and not result["frames"]:
        raise RuntimeError("workload produced no decoded frames")
    return result


def browser(*args, **kwargs):
    return subprocess.run(
        ["agent-browser", "--session", SESSION, *args],
        check=True,
        text=True,
        stdout=subprocess.PIPE,
        **kwargs,
    ).stdout


def wait_for(predicate, message, timeout=30):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if predicate():
            return
        time.sleep(0.1)
    raise RuntimeError(message)


def http_ready():
    try:
        with urllib.request.urlopen(URL, timeout=1) as response:
            return response.status == 200
    except (OSError, urllib.error.URLError):
        return False


def start(command, env, log):
    return subprocess.Popen(
        command, env=env, stdout=log, stderr=subprocess.STDOUT, start_new_session=True
    )


def stop(process):
    if process.poll() is None:
        os.killpg(process.pid, signal.SIGTERM)
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGKILL)
            process.wait()


def latency():
    for index in range(12):
        evaluate("window.benchmarkArmed = true")
        browser("press", "a")
        try:
            wait_for(
                lambda index=index: (
                    evaluate("window.benchmarkLatencies.length") > index
                ),
                "no changed pixel after key event",
                timeout=3,
            )
        except RuntimeError:
            return {
                "status": "n/a: no changed pixel after viewer key event",
                "samples": evaluate("window.benchmarkLatencies"),
            }
        time.sleep(0.2)
    samples = evaluate("window.benchmarkLatencies")
    ordered = sorted(samples)
    return {
        "median_ms": round(statistics.median(samples), 1),
        "p95_ms": round(ordered[math.ceil(len(ordered) * 0.95) - 1], 1),
        "samples": [round(value, 1) for value in samples],
    }


def agent_checks(app_env, window, directory, fixture):
    subprocess.run(["xdotool", "windowfocus", window], env=app_env, check=True)
    marker = directory / "xdotool-result"
    subprocess.run(["xdotool", "key", "ctrl+u"], env=app_env, check=True)
    subprocess.run(
        ["xdotool", "type", "--", f"printf xdotool > {shlex.quote(str(marker))}"],
        env=app_env,
        check=True,
    )
    subprocess.run(["xdotool", "key", "Return"], env=app_env, check=True)
    try:
        wait_for(marker.exists, "xdotool command did not execute", timeout=5)
        xdotool = marker.read_text() == "xdotool"
    except RuntimeError:
        xdotool = False
    marker = directory / "anthropic-result"
    agent_env = app_env.copy()
    agent_env["PYTHONPATH"] = (
        f"{fixture / 'deps'}:{fixture / 'anthropic/computer-use-demo'}"
    )
    result = subprocess.run(
        [sys.executable, str(ROOT / "tools/benchmark/anthropic_agent.py"), str(marker)],
        env=agent_env,
        text=True,
        capture_output=True,
        check=False,
    )
    if result.returncode == 0:
        try:
            wait_for(marker.exists, "Anthropic command did not execute", timeout=5)
            anthropic = marker.read_text() == "anthropic"
        except RuntimeError:
            anthropic = False
    else:
        anthropic = False
        print("Anthropic tool error:", result.stderr, flush=True)
    return {
        "xdotool": "pass" if xdotool else "fail",
        "anthropic_grim": "pass" if anthropic else "fail",
    }


def desktop_env(runtime):
    env = os.environ.copy()
    env.pop("DISPLAY", None)
    env.pop("WAYLAND_DISPLAY", None)
    env.update(XDG_RUNTIME_DIR=runtime)
    xwayland = (
        Path(
            os.environ.get(
                "WAYWIRE_XWAYLAND_PREFIX",
                Path.home() / ".local/share/waywire-xwayland",
            )
        )
        / "bin"
    )
    if (xwayland / "Xwayland").exists():
        env["PATH"] = f"{xwayland}:{env['PATH']}"
    return env


def waywire_command(public_url, app):
    return [
        str(ROOT / "target/release/waywire-gateway"),
        "--listen",
        f"127.0.0.1:{PORT}",
        "--public-url",
        public_url,
        "--compositor",
        str(ROOT / "target/release/waywire-compositor"),
        "--resolution",
        "1280x720",
        "--frame-rate",
        "30",
        "--bitrate",
        "12000",
        "--",
        "sh",
        "-c",
        app,
    ]


def run_desktop(name, fixture):
    with tempfile.TemporaryDirectory(prefix="waywire-benchmark-") as tmp:
        directory = Path(tmp)
        os.chmod(directory, 0o700)
        env = desktop_env(tmp)
        session_env = directory / "session.env"
        app = f"env -0 > {shlex.quote(str(session_env))}; exec xterm"
        processes = []
        with (directory / "desktop.log").open("w+") as log:
            try:
                launched = time.time() * 1000
                if name == "waywire":
                    processes.append(start(waywire_command(URL, app), env, log))
                else:
                    env.update(
                        WLR_BACKENDS="headless",
                        WLR_RENDERER="pixman",
                        LD_LIBRARY_PATH=str(WAYMOTE / "lib/x86_64-linux-gnu"),
                    )
                    startup = f"sh -c {shlex.quote(app)}"
                    processes.append(
                        start([str(WAYMOTE / "bin/labwc"), "-s", startup], env, log)
                    )
                    wait_for(
                        lambda: (directory / "wayland-0").exists(),
                        "labwc socket missing",
                    )
                    env["WAYLAND_DISPLAY"] = "wayland-0"
                    subprocess.run(
                        [
                            "wlr-randr",
                            "--output",
                            "HEADLESS-1",
                            "--custom-mode",
                            "1280x720@30Hz",
                        ],
                        env=env,
                        check=True,
                    )
                    command = [
                        str(WAYMOTE / "bin/waymote-gateway"),
                        "-listen",
                        f"127.0.0.1:{PORT}",
                        "-public-url",
                        URL,
                        "-streamd",
                        str(WAYMOTE / "libexec/waymote-streamd"),
                        "-fixed-width",
                        "1280",
                        "-fixed-height",
                        "720",
                        "-frame-rate",
                        "30",
                        "-bitrate",
                        "12000",
                    ]
                    processes.append(start(command, env, log))
                wait_for(http_ready, f"{name} gateway did not start")
                wait_for(
                    lambda: session_env.exists() and session_env.stat().st_size > 0,
                    f"{name} application did not start",
                )
                app_env = dict(
                    part.split("=", 1)
                    for part in session_env.read_bytes().decode().split("\0")
                    if "=" in part
                )
                wait_for(
                    lambda: (
                        subprocess.run(
                            ["xdotool", "search", "--onlyvisible", "--class", "xterm"],
                            env=app_env,
                            stdout=subprocess.DEVNULL,
                            stderr=subprocess.DEVNULL,
                            check=False,
                        ).returncode
                        == 0
                    ),
                    f"{name} xterm did not map",
                )
                window = subprocess.check_output(
                    ["xdotool", "search", "--onlyvisible", "--class", "xterm"],
                    env=app_env,
                    text=True,
                ).splitlines()[0]
                terminal_pid = int(
                    subprocess.check_output(
                        ["xdotool", "getwindowpid", window], env=app_env, text=True
                    )
                )
                browser("open", URL)
                browser("set", "viewport", "1280", "720", "2")
                wait_for(
                    lambda: json.loads(
                        browser("--json", "eval", "window.benchmarkNonblank")
                    )["data"]["result"],
                    f"{name} viewer did not decode a nonblank frame",
                )
                print(f"{name}: viewer decoded a nonblank frame", flush=True)
                first_frame = evaluate("window.benchmarkFirstFrame")
                if name == "waywire":
                    browser("eval", "document.querySelector('#control-toggle').click()")
                roots = [process.pid for process in processes] + [terminal_pid]
                # Labwc's Xwayland is reparented to PID 1 but stays in its session.
                sessions = [processes[0].pid] if name == "waymote" else []
                time.sleep(2)
                results = {
                    "launch_ms": round(first_frame - launched),
                    "idle": measure(roots, sessions=sessions),
                }
                subprocess.run(
                    ["xdotool", "windowfocus", window], env=app_env, check=True
                )
                results["typing"] = measure(
                    roots,
                    [
                        "xdotool",
                        "type",
                        "--delay",
                        "50",
                        "waywire benchmark typing " * 6,
                    ],
                    app_env,
                    sessions,
                )

                page = directory / "scroll.html"
                page.write_text(
                    "<!doctype html><title>Benchmark scroll</title>"
                    + "".join(
                        f"<p>Line {i}: the quick brown fox jumps over the lazy dog.</p>"
                        for i in range(500)
                    )
                )
                profile = directory / "chromium"
                chrome = start(
                    [
                        "chromium",
                        "--no-sandbox",
                        "--disable-gpu",
                        "--disable-dev-shm-usage",
                        "--ozone-platform=x11",
                        f"--user-data-dir={profile}",
                        "--new-window",
                        page.as_uri(),
                    ],
                    app_env,
                    log,
                )
                processes.append(chrome)
                roots.append(chrome.pid)
                wait_for(
                    lambda: (
                        subprocess.run(
                            [
                                "xdotool",
                                "search",
                                "--onlyvisible",
                                "--name",
                                "Benchmark scroll",
                            ],
                            env=app_env,
                            stdout=subprocess.DEVNULL,
                            stderr=subprocess.DEVNULL,
                            check=False,
                        ).returncode
                        == 0
                    ),
                    f"{name} browser did not map",
                )
                chrome_window = subprocess.check_output(
                    [
                        "xdotool",
                        "search",
                        "--onlyvisible",
                        "--name",
                        "Benchmark scroll",
                    ],
                    env=app_env,
                    text=True,
                ).splitlines()[0]
                subprocess.run(
                    ["xdotool", "windowfocus", chrome_window], env=app_env, check=True
                )
                time.sleep(2)
                results["scroll"] = measure(
                    roots,
                    ["xdotool", "key", "--repeat", "80", "--delay", "100", "Down"],
                    app_env,
                    sessions,
                )
                results["scroll"]["quality"] = evaluate("window.benchmarkQuality")
                stop(chrome)
                subprocess.run(
                    ["xdotool", "windowsize", window, "1100", "650"],
                    env=app_env,
                    check=True,
                )
                subprocess.run(
                    ["xdotool", "windowmove", window, "0", "0"], env=app_env, check=True
                )
                subprocess.run(
                    ["xdotool", "windowfocus", window], env=app_env, check=True
                )
                if name == "waywire":
                    browser("eval", "document.querySelector('#control-toggle').click()")
                browser("click", "#display")
                time.sleep(1)
                results["latency"] = latency()
                browser("open", "about:blank")
                results["agents"] = agent_checks(app_env, window, directory, fixture)
                print(json.dumps({name: results}, indent=2), flush=True)
                return results
            except Exception:
                log.flush()
                print((directory / "desktop.log").read_text(), flush=True)
                raise
            finally:
                try:
                    browser("open", "about:blank")
                finally:
                    for process in reversed(processes):
                        stop(process)


def slow_link(encoder):
    """Stream an animation with `encoder` while the viewer's link is slow, then clear it."""
    viewer = f"http://127.0.0.1:{PORT + 1}"
    with tempfile.TemporaryDirectory(prefix="waywire-slow-link-") as tmp:
        directory = Path(tmp)
        os.chmod(directory, 0o700)
        env = desktop_env(tmp)
        env["WAYWIRE_ENCODER"] = encoder
        page = directory / "motion.html"
        page.write_text(MOTION_PAGE)
        app = (
            "exec chromium --no-sandbox --disable-gpu --disable-dev-shm-usage "
            f"--ozone-platform=x11 --user-data-dir={directory / 'chromium'} "
            f"--start-maximized --app={page.as_uri()}"
        )
        link = Link(PORT + 1, ("127.0.0.1", PORT))
        with (directory / "desktop.log").open("w+") as log:
            gateway = start(waywire_command(viewer, app), env, log)
            try:
                wait_for(http_ready, "waywire gateway did not start")
                browser("open", viewer)
                browser("set", "viewport", "1280", "720", "2")
                wait_for(
                    lambda: evaluate("window.benchmarkNonblank"),
                    "slow-link viewer did not decode a nonblank frame",
                )
                # Only the viewer with input control drives quality.
                browser("click", "#display")
                time.sleep(5)
                full = evaluate("window.benchmarkQuality")
                samples = []
                # Capture times are the host's monotonic clock; the viewer runs on
                # the same host, so wall time minus this offset dates each decode.
                offset_ms = (time.time() - time.monotonic()) * 1000
                decoded = evaluate("window.benchmarkDecodes.length")

                def watch(phase, seconds, done=lambda quality: False):
                    nonlocal decoded
                    state = (
                        "({time: performance.now(), draws: window.benchmarkDraws,"
                        " bytes: window.benchmarkSockets['/stream'] ?? 0,"
                        " quality: window.benchmarkQuality})"
                    )
                    previous = evaluate(state)
                    started = time.monotonic()
                    while time.monotonic() - started < seconds:
                        time.sleep(1)
                        current = evaluate(state)
                        elapsed = (current["time"] - previous["time"]) / 1000
                        quality = current["quality"]
                        decodes = evaluate(f"window.benchmarkDecodes.slice({decoded})")
                        decoded += len(decodes)
                        ages = [
                            wall - (capture / 1000 + offset_ms)
                            for wall, capture in decodes
                        ]
                        samples.append(
                            {
                                "phase": phase,
                                "seconds": round(time.monotonic() - started, 1),
                                "fps": round(
                                    (current["draws"] - previous["draws"]) / elapsed, 1
                                ),
                                "kbps": round(
                                    (current["bytes"] - previous["bytes"])
                                    * 8
                                    / 1000
                                    / elapsed
                                ),
                                "scale": quality["scale"],
                                "bitrate": quality["bitrate"],
                                "max_fps": quality["fps"],
                                "age_ms": round(statistics.median(ages))
                                if ages
                                else None,
                            }
                        )
                        previous = current
                        if done(quality):
                            return samples[-1]["seconds"]
                    return None

                link.limit(SLOW_LINK_KBPS)
                watch("slow", SLOW_LINK_SECONDS)
                link.limit(None)
                recovery = watch(
                    "recovery",
                    RECOVERY_SECONDS,
                    lambda quality: all(
                        quality[key] == full[key] for key in ("scale", "bitrate", "fps")
                    ),
                )
                slow = [sample for sample in samples if sample["phase"] == "slow"]
                tail = slow[-15:]
                result = {
                    "lowest_scale": min(sample["scale"] for sample in slow),
                    "full_scale": full["scale"],
                    "lowest_bitrate": min(sample["bitrate"] for sample in slow),
                    "full_bitrate": full["bitrate"],
                    "slow_fps": round(statistics.mean(s["fps"] for s in tail), 1),
                    "slow_kbps": round(statistics.mean(s["kbps"] for s in tail)),
                    "slow_age_ms": round(statistics.median(ages))
                    if (ages := [s["age_ms"] for s in tail if s["age_ms"] is not None])
                    else None,
                    "recovery_s": recovery,
                    "samples": samples,
                }
                print(
                    json.dumps(
                        {f"slow link {encoder}": {**result, "samples": len(samples)}}
                    ),
                    flush=True,
                )
                return result
            except Exception:
                log.flush()
                print((directory / "desktop.log").read_text(), flush=True)
                raise
            finally:
                try:
                    browser("open", "about:blank")
                finally:
                    stop(gateway)
                    link.close()


def check_slow_link(encoder, result):
    if result is None:
        return [f"slow link {encoder}: unavailable"]
    failures = []
    if result["lowest_scale"] >= result["full_scale"]:
        failures.append(f"slow link {encoder}: scale stayed at {result['full_scale']}")
    if result["lowest_bitrate"] >= result["full_bitrate"]:
        failures.append(
            f"slow link {encoder}: bitrate stayed at {result['full_bitrate']}"
        )
    if result["slow_fps"] < SLOW_LINK_MIN_FPS:
        failures.append(
            f"slow link {encoder} fps: {result['slow_fps']} < {SLOW_LINK_MIN_FPS}"
        )
    if result["slow_age_ms"] is None or result["slow_age_ms"] > SLOW_LINK_MAX_AGE_MS:
        failures.append(
            f"slow link {encoder} frame age: {result['slow_age_ms']} ms > {SLOW_LINK_MAX_AGE_MS} ms"
        )
    if result["recovery_s"] is None:
        failures.append(
            f"slow link {encoder}: quality did not recover within {RECOVERY_SECONDS} s"
        )
    return failures


def check_regressions(results):
    waywire, waymote = results["waywire"], results["waymote"]
    comparisons = [
        ("launch ms", waywire["launch_ms"], waymote["launch_ms"], 1.25, 200),
        (
            "idle CPU %",
            waywire["idle"]["cpu_percent"],
            waymote["idle"]["cpu_percent"],
            1.2,
            2,
        ),
        (
            "idle RSS MiB",
            waywire["idle"]["rss_mib"],
            waymote["idle"]["rss_mib"],
            1.15,
            10,
        ),
    ]
    for workload in ("typing", "scroll"):
        comparisons.extend(
            [
                (
                    f"{workload} CPU %",
                    waywire[workload]["cpu_percent"],
                    waymote[workload]["cpu_percent"],
                    1.2,
                    2,
                ),
                (
                    f"{workload} RSS MiB",
                    waywire[workload]["rss_mib"],
                    waymote[workload]["rss_mib"],
                    1.15,
                    10,
                ),
                (
                    f"{workload} stream kbps",
                    waywire[workload]["kbps_by_socket"].get("/stream"),
                    waymote[workload]["kbps_by_socket"].get("/stream"),
                    1.25,
                    20,
                ),
            ]
        )
    for metric in ("median_ms", "p95_ms"):
        comparisons.append(
            (
                f"latency {metric}",
                waywire["latency"].get(metric),
                waymote["latency"].get(metric),
                1.25,
                10,
            )
        )
    failures = []
    for name, actual, baseline, multiplier, allowance in comparisons:
        if actual is None or baseline is None:
            failures.append(f"{name}: unavailable")
        elif actual > baseline * multiplier + allowance:
            failures.append(
                f"{name}: {actual} > {baseline * multiplier + allowance:.1f} (Waymote {baseline})"
            )
    for name, status in waymote["agents"].items():
        if status == "pass" and waywire["agents"][name] != "pass":
            failures.append(f"{name}: waywire failed, Waymote passed")
    for encoder in ENCODERS:
        failures.extend(
            check_slow_link(encoder, results.get("slow_link", {}).get(encoder))
        )
    return failures


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--check",
        action="store_true",
        help="fail on regressions beyond the documented tolerances",
    )
    args = parser.parse_args()
    with tempfile.TemporaryDirectory(prefix="benchmark-agent-") as tmp:
        fixture = Path(tmp)
        subprocess.run(
            [
                "git",
                "clone",
                "--quiet",
                "--filter=blob:none",
                "--no-checkout",
                "https://github.com/anthropics/anthropic-quickstarts.git",
                str(fixture / "anthropic"),
            ],
            check=True,
        )
        subprocess.run(
            [
                "git",
                "-C",
                str(fixture / "anthropic"),
                "checkout",
                "--quiet",
                "1a18c340ca42b9137d991637afe867bdcf70a9ce",
            ],
            check=True,
        )
        subprocess.run(
            [
                "uv",
                "pip",
                "install",
                "--quiet",
                "--target",
                str(fixture / "deps"),
                "anthropic==2.3.0",
            ],
            check=True,
        )
        try:
            browser(
                "--init-script",
                str(ROOT / "tools/benchmark/instrument.js"),
                "open",
                "about:blank",
            )
            results = {
                name: run_desktop(name, fixture) for name in ("waywire", "waymote")
            }
            results["slow_link"] = {encoder: slow_link(encoder) for encoder in ENCODERS}
            if args.check:
                failures = check_regressions(results)
                if failures:
                    print(
                        "Regression check failed:\n" + "\n".join(failures), flush=True
                    )
                    return 1
                print("Regression check passed", flush=True)
        finally:
            subprocess.run(
                ["agent-browser", "--session", SESSION, "close"],
                check=False,
                stdout=subprocess.DEVNULL,
            )
    return 0


if __name__ == "__main__":
    sys.exit(main())
