#!/usr/bin/env python3
"""Bounded terminal fixture for the local Socket verification."""
from __future__ import annotations

import json
import os
import select
import sys
import termios
import time
from pathlib import Path

STATE = Path("/home/sprite/fixture-state.json")
MODE = Path("/tmp/socket-local/fixture-mode")
MAX_KEYS = 512


def save(keys: str, frames: int) -> None:
    temporary = STATE.with_suffix(".tmp")
    temporary.write_text(json.dumps({"keys": keys, "frames": frames}), encoding="utf-8")
    temporary.replace(STATE)


def draw(keys: str, frame: int, motion: bool) -> None:
    offset = frame % 37 if motion else 0
    lines = [
        "SOCKET LOCAL REPRODUCIBLE FIXTURE",
        "H.264 I444 / full-range / compositor source",
        f"mode={'motion' if motion else 'quiet'} frame={frame:06d} keys={keys!r}",
        "",
    ]
    for row in range(24):
        lines.append(
            f"{row + offset:04d}  callback {row:02d}: RGB 12ab AB[]{{}} moving verification row"
        )
    sys.stdout.write("\x1b[H\x1b[2J" + "\n".join(lines) + "\n")
    sys.stdout.flush()


def main() -> int:
    descriptor = sys.stdin.fileno()
    original = termios.tcgetattr(descriptor)
    raw = termios.tcgetattr(descriptor)
    raw[3] &= ~(termios.ICANON | termios.ECHO)
    raw[6][termios.VMIN] = 0
    raw[6][termios.VTIME] = 0
    termios.tcsetattr(descriptor, termios.TCSANOW, raw)
    keys = ""
    frame = 0
    last_motion = None
    last_draw = 0.0
    save(keys, frame)
    sys.stdout.write("\x1b[?25l")
    sys.stdout.flush()
    try:
        while True:
            motion = MODE.exists() and MODE.read_text(encoding="ascii").strip() == "motion"
            now = time.monotonic()
            if motion and now - last_draw >= 1 / 30:
                frame += 1
            if motion != last_motion or (motion and now - last_draw >= 1 / 30):
                draw(keys, frame, motion)
                save(keys, frame)
                last_draw = now
                last_motion = motion
            readable, _, _ = select.select([descriptor], [], [], 0.02)
            if readable:
                data = os.read(descriptor, 64).decode("utf-8", "replace")
                keys = (keys + data)[-MAX_KEYS:]
                save(keys, frame)
                draw(keys, frame, motion)
                last_draw = time.monotonic()
    finally:
        sys.stdout.write("\x1b[?25h")
        sys.stdout.flush()
        termios.tcsetattr(descriptor, termios.TCSANOW, original)


if __name__ == "__main__":
    raise SystemExit(main())
