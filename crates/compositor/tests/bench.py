#!/usr/bin/env python3
"""Linux server-path benchmark: real wl_shm -> Pixman -> FFmpeg -> drained RTP.

Run from the repository root after a release build. No browser or network latency
is measured. JSONL reports compositor/encoder CPU separately (100% = one core).
Compare saved binaries in alternating order; do not run builds during measurement.
"""
import argparse
import json
import os
from pathlib import Path
import re
import runpy
import select
import socket
import statistics
import struct
import subprocess
import tempfile
import time


def cpu_seconds(pid):
    fields = Path(f"/proc/{pid}/stat").read_text().rsplit(") ", 1)[1].split()
    return (int(fields[11]) + int(fields[12])) / os.sysconf("SC_CLK_TCK")


def encoder_pid(pid):
    for task in Path(f"/proc/{pid}/task").iterdir():
        for child in (task / "children").read_text().split():
            if Path(f"/proc/{child}/comm").read_text().strip() == "ffmpeg":
                return int(child)
    raise RuntimeError("FFmpeg did not start")


def measure(binary, receiver, directory, mode, args):
    width, height = map(int, args.resolution.split("x"))
    env = dict(os.environ, WAYWIRE_BENCH=mode, RUST_LOG="info")
    env.pop("WAYWIRE_SESSION", None)
    log = directory / "compositor.log"
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as udp, log.open("wb") as errors:
        udp.bind(("127.0.0.1", 0))
        process = subprocess.Popen([
            str(binary), "--rtp-port", str(udp.getsockname()[1]),
            "--resolution", args.resolution, "--frame-rate", str(args.fps),
        ], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=errors, env=env)
        client = None
        try:
            deadline = time.monotonic() + 10
            while not (match := re.search(r"socket=([^\s]+)", log.read_text())):
                if process.poll() is not None or time.monotonic() > deadline:
                    raise RuntimeError(log.read_text())
                time.sleep(0.01)
            client = subprocess.Popen([str(receiver), str(width), str(height)],
                env={**env, "WAYLAND_DISPLAY": match[1]}, stdout=subprocess.DEVNULL, stderr=errors)
            pending = b""
            frames = []
            acknowledged = set()
            packets = 0
            measured_bytes = 0
            start = time.monotonic() + args.warmup
            end = start + args.seconds
            cpu_start = None
            while time.monotonic() < end:
                now = time.monotonic()
                if cpu_start is None and now >= start:
                    ffmpeg = encoder_pid(process.pid)
                    cpu_start = (now, cpu_seconds(process.pid), cpu_seconds(ffmpeg))
                if client.poll() is not None:
                    raise RuntimeError("benchmark client exited: " + log.read_text())
                for stream in select.select([process.stdout, udp], [], [], 0.02)[0]:
                    if stream is udp:
                        packet = udp.recv(65536)
                        if now >= start:
                            measured_bytes += len(packet)
                        packets += 1
                        continue
                    chunk = os.read(process.stdout.fileno(), 65536)
                    if not chunk:
                        raise RuntimeError(log.read_text())
                    pending += chunk
                    while len(pending) >= 8:
                        version, kind, reserved, length = struct.unpack_from("<BBHI", pending)
                        assert version in (8, 9) and reserved == 0
                        if len(pending) < 8 + length:
                            break
                        payload, pending = pending[8:8+length], pending[8+length:]
                        if kind != 2:
                            continue
                        generation = struct.unpack_from("<I", payload)[0]
                        # Model the gateway caching the initial keyframe; otherwise
                        # the compositor deliberately repaints even an idle output.
                        if generation not in acknowledged:
                            process.stdin.write(struct.pack("<BBHIIB", version, 11, 0, 5, generation, 1))
                            process.stdin.flush()
                            acknowledged.add(generation)
                        captured, sequence = struct.unpack_from("<QQ", payload, 8)
                        if start <= captured / 1e9 < end:
                            frames.append((captured, sequence))
            elapsed = time.monotonic() - cpu_start[0]
            compositor_cpu = cpu_seconds(process.pid) - cpu_start[1]
            encoder_cpu = cpu_seconds(ffmpeg) - cpu_start[2]
            status = Path(f"/proc/{process.pid}/status").read_text()
            intervals = [(b[0] - a[0]) / 1e6 for a, b in zip(frames, frames[1:])]
            assert packets and acknowledged, "no encoded output"
            assert mode == "idle" or len(frames) > 1, "animation did not produce frames"
            assert mode != "idle" or not frames, "idle output keeps encoding"
            return {
                "binary": str(binary), "mode": mode, "resolution": args.resolution,
                "target_fps": args.fps, "seconds": args.seconds, "frames": len(frames),
                "submitted_fps": round(len(frames) / args.seconds, 2),
                "rtp_kbps": round(measured_bytes * 8 / args.seconds / 1000, 2),
                "interval_p50_ms": round(statistics.median(intervals), 2) if intervals else None,
                "interval_p95_ms": round(sorted(intervals)[int((len(intervals)-1)*0.95)], 2) if intervals else None,
                "replaced_frames": sum(b[1] - a[1] - 1 for a, b in zip(frames, frames[1:])),
                "compositor_cpu_percent": round(compositor_cpu / elapsed * 100, 2),
                "encoder_cpu_percent": round(encoder_cpu / elapsed * 100, 2),
                "compositor_cpu_ms_per_frame": round(compositor_cpu * 1000 / len(frames), 2) if frames else None,
                "compositor_rss_mib": round(int(re.search(r"VmRSS:\s+(\d+)", status)[1]) / 1024, 2),
                "compositor_peak_rss_mib": round(int(re.search(r"VmHWM:\s+(\d+)", status)[1]) / 1024, 2),
            }
        finally:
            if client:
                client.terminate()
                client.wait(timeout=5)
            process.stdin.close()
            process.wait(timeout=10)
            if process.returncode:
                raise RuntimeError(log.read_text())


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binaries", nargs="*", type=Path, default=[Path("target/release/waywire-compositor")])
    parser.add_argument("--resolution", default="1920x1080")
    parser.add_argument("--fps", type=int, default=60)
    parser.add_argument("--seconds", type=float, default=6)
    parser.add_argument("--warmup", type=float, default=2)
    parser.add_argument("--repeat", type=int, default=3)
    parser.add_argument("--modes", nargs="+", choices=["full", "small", "idle"], default=["full", "small", "idle"])
    args = parser.parse_args()
    if args.seconds <= 0 or args.warmup <= 0 or args.repeat < 1:
        parser.error("seconds, warmup, and repeat must be positive")
    build_client = runpy.run_path(str(Path(__file__).with_name("native-scene.py")))["build_client"]
    with tempfile.TemporaryDirectory(prefix="waywire-bench-") as temporary:
        directory = Path(temporary)
        receiver = build_client(directory)
        binaries = [p.resolve(strict=True) for p in args.binaries]
        for run in range(args.repeat):
            for mode in args.modes:
                for binary in binaries if run % 2 == 0 else reversed(binaries):
                    print(json.dumps({"run": run + 1, **measure(binary, receiver, directory, mode, args)}), flush=True)


if __name__ == "__main__":
    main()
