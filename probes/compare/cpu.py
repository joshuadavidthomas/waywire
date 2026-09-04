"""Sample whole-Sprite CPU/memory. Run beside a browser recording, not during idle tests."""

from __future__ import annotations

import argparse
import datetime
import json
import time
from pathlib import Path


def snapshot():
    lines = Path("/proc/stat").read_text().splitlines()
    fields = [int(value) for value in lines[0].split()[1:9]]
    memory = {}
    for line in Path("/proc/meminfo").read_text().splitlines():
        key, value = line.split(":", 1)
        if key in {"MemTotal", "MemAvailable"}:
            memory[key] = int(value.split()[0]) * 1024
    return {
        "at": datetime.datetime.now(datetime.timezone.utc).isoformat(),
        "monotonic": time.monotonic(),
        "total": sum(fields),
        "idle": fields[3] + fields[4],
        "cpuCount": sum(line.startswith("cpu") and not line.startswith("cpu ") for line in lines),
        "memory": memory,
    }


parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--seconds", type=int, default=90)
parser.add_argument("--output", required=True)
args = parser.parse_args()
if not 1 <= args.seconds <= 300:
    parser.error("--seconds must be between 1 and 300")

# Refuse to overwrite earlier evidence.
with Path(args.output).open("x") as output:
    previous = snapshot()
    samples = []
    deadline = time.monotonic() + args.seconds
    while time.monotonic() < deadline:
        time.sleep(1)
        current = snapshot()
        total = current["total"] - previous["total"]
        busy = total - (current["idle"] - previous["idle"])
        fraction = busy / total if total > 0 else None
        samples.append({
            "at": current["at"],
            "intervalSeconds": current["monotonic"] - previous["monotonic"],
            "cpuCount": current["cpuCount"],
            "cpuPercentAllCpus": fraction * 100 if fraction is not None else None,
            "busyCoreEquivalent": fraction * current["cpuCount"] if fraction is not None else None,
            "memoryBytes": current["memory"],
        })
        previous = current
    json.dump({
        "schema": 1,
        "scope": "Whole Sprite as reported by /proc, including this sampler and unrelated processes. Not a per-service measurement.",
        "alignment": "UTC timestamps; browser and Sprite clocks may differ. Start this before recording and trim to the browser interval only after checking clock offset.",
        "samples": samples,
    }, output, indent=2)
print(args.output)
