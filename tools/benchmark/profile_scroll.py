#!/usr/bin/env python3
"""Attribute the existing desktop workloads' memory and optionally sample scroll CPU.

This diagnostic calls benchmark.run_desktop without changing its workloads or
limits. It omits the unrelated agent checks and does not replace --check.
"""

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import time

import benchmark


def memory(pid):
    directory = Path(f"/proc/{pid}")
    command = directory.joinpath("cmdline").read_bytes().replace(b"\0", b" ").decode()
    totals = {}
    mappings = {}
    name = ""
    for line in directory.joinpath("smaps").read_text().splitlines():
        if re.match(r"^[0-9a-f]+-[0-9a-f]+ ", line):
            fields = line.split(maxsplit=5)
            name = fields[5] if len(fields) == 6 else "[anonymous]"
            continue
        key, _, value = line.partition(":")
        if key not in (
            "Rss",
            "Pss",
            "Private_Dirty",
            "Private_Clean",
            "Shared_Dirty",
            "Shared_Clean",
            "Anonymous",
            "AnonHugePages",
            "Swap",
        ):
            continue
        amount = int(value.split()[0])
        totals[key] = totals.get(key, 0) + amount
        mapping = mappings.setdefault(name, {})
        mapping[key] = mapping.get(key, 0) + amount
    environment = dict(
        part.split(b"=", 1)
        for part in directory.joinpath("environ").read_bytes().split(b"\0")
        if b"=" in part
    )
    keys = (
        "DISPLAY",
        "WAYLAND_DISPLAY",
        "XDG_SESSION_TYPE",
        "XDG_CURRENT_DESKTOP",
        "LD_LIBRARY_PATH",
        "GDK_SCALE",
        "GDK_DPI_SCALE",
        "QT_SCALE_FACTOR",
        "MALLOC_ARENA_MAX",
    )
    return {
        "pid": pid,
        "command": command,
        "totals_kib": totals,
        "mappings_kib": mappings,
        "environment": {
            key: environment[key.encode()].decode()
            for key in keys
            if key.encode() in environment
        },
        "status": directory.joinpath("status").read_text(),
        "limits": directory.joinpath("limits").read_text(),
    }


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--desktop", choices=("waywire", "waymote"), required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument(
        "--bin-dir",
        type=Path,
        default=benchmark.ROOT / "target/release",
        help="Waywire binaries to profile, including saved control builds",
    )
    parser.add_argument(
        "--perf",
        action="store_true",
        help="record scroll stacks separately from resource-only runs",
    )
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=True)
    original_command = benchmark.waywire_command

    def waywire_command(public_url, app):
        command = original_command(public_url, app)
        command[0] = str(args.bin_dir.resolve() / "waywire-gateway")
        command[command.index("--compositor") + 1] = str(
            args.bin_dir.resolve() / "waywire-compositor"
        )
        return command

    benchmark.waywire_command = waywire_command
    if args.desktop == "waywire":
        binaries = {}
        for name in ("waywire-gateway", "waywire-compositor"):
            path = (args.bin_dir / name).resolve()
            with path.open("rb") as binary:
                digest = hashlib.file_digest(binary, "sha256").hexdigest()
            binaries[name] = {"path": str(path), "sha256": digest}
        (args.output / "binaries.json").write_text(json.dumps(binaries, indent=2))
    original_measure = benchmark.measure
    worker = None

    def measure(roots, action=None, env=None, sessions=()):
        phase = "idle" if action is None else "typing" if "type" in action else "scroll"
        recorder = None
        processes = benchmark.process_snapshot(roots, sessions)
        if args.perf and phase == "scroll":
            executable = "waywire-compositor" if args.desktop == "waywire" else "labwc"
            compositor = next(
                pid
                for pid, _ in processes
                if Path(
                    Path(f"/proc/{pid}/cmdline").read_bytes().split(b"\0")[0].decode()
                ).name
                == executable
            )
            recorder = subprocess.Popen(
                [
                    "perf",
                    "record",
                    "-e",
                    "cpu-clock:u",
                    "-F",
                    "499",
                    "-g",
                    "--call-graph",
                    "dwarf,8192",
                    "-p",
                    str(compositor),
                    "-o",
                    str(args.output / "scroll.perf.data"),
                ],
                stderr=subprocess.DEVNULL,
            )
        started = time.monotonic()
        try:
            result = original_measure(roots, action, env, sessions)
        finally:
            if recorder is not None:
                recorder.terminate()
                recorder.wait()
        elapsed = time.monotonic() - started
        census = []
        for (pid, birth), (ticks, _) in benchmark.process_snapshot(
            roots, sessions
        ).items():
            try:
                process = memory(pid)
                if (pid, birth) in processes:
                    process["cpu_percent"] = round(
                        (ticks - processes[pid, birth][0])
                        / os.sysconf("SC_CLK_TCK")
                        / elapsed
                        * 100,
                        3,
                    )
                census.append(process)
            except (FileNotFoundError, ProcessLookupError):
                continue
        (args.output / f"{phase}-memory.json").write_text(json.dumps(census, indent=2))
        if phase == "scroll":
            compositor = next(
                (
                    item
                    for item in census
                    if item["command"].split()
                    and Path(item["command"].split()[0]).name == "waywire-compositor"
                ),
                None,
            )
            if compositor is not None:
                (args.output / "desktop.log").write_text(
                    Path(f"/proc/{compositor['pid']}/fd/2").read_text()
                )
            (args.output / "windows.txt").write_text(
                subprocess.check_output(
                    ["xwininfo", "-root", "-tree"], env=env, text=True
                )
            )
            for name, command in (
                ("output.txt", ["xrandr", "--current"]),
                ("xresources.txt", ["xrdb", "-query"]),
            ):
                if shutil.which(command[0]) is None:
                    continue
                completed = subprocess.run(
                    command, env=env, text=True, capture_output=True, check=False
                )
                (args.output / name).write_text(completed.stdout + completed.stderr)
        print(
            json.dumps({"desktop": args.desktop, "phase": phase, **result}), flush=True
        )
        return result

    benchmark.measure = measure
    benchmark.agent_checks = lambda *args: {"omitted": "memory and CPU diagnostic"}
    try:
        benchmark.browser(
            "--init-script",
            str(benchmark.ROOT / "tools/benchmark/instrument.js"),
            "open",
            "about:blank",
        )
        worker = benchmark.instrument_video_worker()
        result = benchmark.run_desktop(args.desktop, Path("/tmp"))
        (args.output / "results.json").write_text(json.dumps(result, indent=2))
    finally:
        if worker is not None:
            worker.kill()
            worker.wait()
        subprocess.run(
            ["agent-browser", "--session", benchmark.SESSION, "close"],
            check=False,
            stdout=subprocess.DEVNULL,
        )
    if args.perf:
        with (args.output / "scroll.perf.txt").open("w") as report:
            subprocess.run(
                [
                    "perf",
                    "report",
                    "-i",
                    str(args.output / "scroll.perf.data"),
                    "--stdio",
                    "--no-children",
                    "--percent-limit",
                    "0.5",
                ],
                stdout=report,
                env={**os.environ, "DEBUGINFOD_URLS": ""},
                check=True,
            )


if __name__ == "__main__":
    main()
