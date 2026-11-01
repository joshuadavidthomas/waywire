#!/usr/bin/env python3
"""Run the dogfood task replays (replay.py) in Debian containers.

Usage: suite.py [--distro bookworm|trixie|all] [--out DIR] [--no-build] [CELL ...]

Builds the acceptance image for each distro (Dockerfile.debian), mounts the host
release binaries read-only, and runs the chosen cells, each in a fresh waywire
session, inside one disposable container per distro. Results and per-cell
evidence go to DIR/<distro> (default target/acceptance). Requires Docker and
`just build`; the binaries must run on Debian 12's glibc.
"""

import argparse
import json
import os
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
DISTROS = ("bookworm", "trixie")


def docker(*args, **kwargs):
    return subprocess.run(["sudo", "docker", *args], check=True, text=True, **kwargs)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--distro", choices=(*DISTROS, "all"), default="all")
    parser.add_argument("--out", type=Path, default=ROOT / "target/acceptance")
    parser.add_argument("--no-build", action="store_true", help="reuse built images")
    parser.add_argument("cells", nargs="*", help="cell name prefixes, e.g. T1 or T4/cua")
    args = parser.parse_args()
    for binary in ("waywire-gateway", "waywire-compositor"):
        if not (ROOT / "target/release" / binary).exists():
            raise SystemExit(f"target/release/{binary} is missing; run `just build`")
    failed = False
    for distro in DISTROS if args.distro == "all" else (args.distro,):
        image = f"waywire-acceptance-{distro}"
        started = time.monotonic()
        if not args.no_build:
            docker(
                "build",
                "--build-arg",
                f"RELEASE={distro}",
                "-f",
                str(ROOT / "tools/acceptance/Dockerfile.debian"),
                "-t",
                image,
                str(ROOT / "tools/acceptance"),
            )
        built = time.monotonic()
        out = args.out / distro
        out.mkdir(parents=True, exist_ok=True)
        os.chmod(out, 0o777)
        (out / "results.jsonl").unlink(missing_ok=True)
        print(f"== {distro}: running cells", flush=True)
        status = subprocess.run(
            [
                "sudo",
                "docker",
                "run",
                "--rm",
                "--init",
                "--shm-size=1g",
                "-u",
                "1000:1000",
                "-v",
                f"{ROOT}:/repo:ro",
                "-v",
                f"{out.resolve()}:/results",
                image,
                "python3",
                "/repo/tools/acceptance/replay.py",
                "--out",
                "/results",
                *args.cells,
            ],
            check=False,
        ).returncode
        finished = time.monotonic()
        timing = {
            "distro": distro,
            "build_seconds": round(built - started, 1),
            "run_seconds": round(finished - built, 1),
            "exit": status,
        }
        (out / "timing.json").write_text(json.dumps(timing) + "\n")
        print(f"== {distro}: {json.dumps(timing)}", flush=True)
        failed |= status != 0
    return 1 if failed else 0


if __name__ == "__main__":
    sys.exit(main())
