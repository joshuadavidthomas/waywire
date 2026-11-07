#!/usr/bin/env python3
"""Compare OpenH264 settings at conservatively matched decoded quality.

Build the profile_openh264 Rust example first, then provide a corpus recorded
with capture_openh264.py. This is an isolated codec study, not --check.
Each setting searches integer QPs downward from baseline+3 until every quality
metric passes. Allowed numerical loss is 0.1 dB, or 0.25 dB on the worst frame.
Those are study matching tolerances, not production acceptance thresholds.
"""

import argparse
import csv
import hashlib
import io
import json
import subprocess
from pathlib import Path

METRICS = (
    "psnr_y",
    "psnr_u",
    "psnr_v",
    "psnr_edge",
    "psnr_changed_y",
    "psnr_changed_uv",
    "psnr_worst",
)


def matches(candidate, baseline):
    return all(
        candidate[key] >= baseline[key] - (0.25 if key == "psnr_worst" else 0.1)
        for key in METRICS
    )


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--corpus", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--repeats", type=int, default=3)
    parser.add_argument(
        "--variants",
        nargs="+",
        choices=("background", "medium", "screen", "rc-off"),
        default=("background", "medium", "screen", "rc-off"),
    )
    parser.add_argument(
        "--binary",
        type=Path,
        default=Path(__file__).resolve().parents[2]
        / "target/release/examples/profile_openh264",
    )
    args = parser.parse_args()
    args.output.mkdir(parents=True, exist_ok=False)
    manifest = json.loads((args.corpus / "manifest.json").read_text())
    width, height, fps = (manifest[key] for key in ("width", "height", "fps"))
    records, selected, cache = [], [], {}

    def run(clip, variant, qp, cap, repeats, suffix):
        name = f"{clip}-{variant}-q{qp}-cap{cap}-{suffix}"
        command = [
            str(args.binary.resolve()),
            str((args.corpus / f"{clip}.bgra").resolve()),
            str(width),
            str(height),
            str(fps),
            variant,
            str(qp),
            str(cap),
            str(repeats),
        ]
        with (args.output / f"{name}.log").open("w") as log:
            result = subprocess.run(
                command, text=True, stdout=subprocess.PIPE, stderr=log, check=True
            )
        (args.output / f"{name}.csv").write_text(result.stdout)
        row = next(csv.DictReader(io.StringIO(result.stdout)))
        row = {
            key: value if key == "variant" else float(value)
            for key, value in row.items()
        }
        row.update(clip=clip, phase=suffix, command=command)
        records.append(row)
        (args.output / "records.json").write_text(json.dumps(records, indent=2))
        return row

    def probe(clip, variant, qp, cap):
        key = (clip, variant, qp, cap)
        if key not in cache:
            cache[key] = run(clip, variant, qp, cap, 1, "search")
        return cache[key]

    # Normal rate-capped floor, then pinned high-quality and normal-QP controls.
    for cap, target_qp in ((12_000_000, 26), (0, 14), (0, 26)):
        for clip in manifest["clips"]:
            target = probe(clip, "baseline", target_qp, cap)
            for variant in args.variants:
                if variant == "rc-off" and cap:
                    continue
                found = None
                for qp in range(target_qp + 3, -1, -1):
                    candidate = probe(clip, variant, qp, cap)
                    if matches(candidate, target):
                        found = qp
                        break
                if found is None:
                    selected.append(
                        {
                            "clip": clip,
                            "variant": variant,
                            "cap": cap,
                            "target_qp": target_qp,
                            "status": "no quality match",
                        }
                    )
                    continue
                # Remeasure the baseline immediately before the selected candidate.
                baseline = run(
                    clip, "baseline", target_qp, cap, args.repeats, f"control-{variant}"
                )
                candidate = run(
                    clip, variant, found, cap, args.repeats, f"matched-q{target_qp}"
                )
                assert matches(candidate, baseline), (
                    "quality changed on identical replay"
                )
                comparison = {
                    "clip": clip,
                    "variant": variant,
                    "cap": cap,
                    "target_qp": target_qp,
                    "selected_qp": found,
                    "baseline": baseline,
                    "candidate": candidate,
                    "cpu_change_percent": (
                        candidate["cpu_ms_frame"] / baseline["cpu_ms_frame"] - 1
                    )
                    * 100,
                    "bytes_change_percent": (candidate["bytes"] / baseline["bytes"] - 1)
                    * 100,
                    "quality_deltas_db": {
                        key: candidate[key] - baseline[key] for key in METRICS
                    },
                }
                selected.append(comparison)
                (args.output / "matches.json").write_text(
                    json.dumps(selected, indent=2)
                )
                print(
                    f"{clip} cap={cap} target={target_qp} {variant} qp={found}: "
                    f"CPU {comparison['cpu_change_percent']:+.1f}%, "
                    f"bytes {comparison['bytes_change_percent']:+.1f}%",
                    flush=True,
                )
    with args.binary.open("rb") as binary:
        checksum = hashlib.file_digest(binary, "sha256").hexdigest()
    (args.output / "provenance.json").write_text(
        json.dumps(
            {
                "binary": str(args.binary.resolve()),
                "sha256": checksum,
                "manifest": manifest,
                "repeats": args.repeats,
                "variants": args.variants,
                "matching_tolerance_db": 0.1,
                "worst_frame_tolerance_db": 0.25,
            },
            indent=2,
        )
    )
    (args.output / "matches.json").write_text(json.dumps(selected, indent=2))


if __name__ == "__main__":
    main()
