#!/usr/bin/env python3
"""Linux server-path benchmark: real wl_shm -> Pixman -> FFmpeg -> drained RTP.

Run from the repository root after a release build. No browser or network latency
is measured. JSONL reports compositor/encoder CPU separately (100% = one core).
Compare saved binaries in alternating order; do not run builds during measurement.
Submitted is the capture-time cohort; complete_encoded_fps counts validated RTP
AU arrivals in the wall-time window. Freeze the producer and drain separately.
Capture sequence gaps infer attempts/replacements only over the observed span.
"""
import argparse
from collections import Counter, defaultdict, deque
import json
import os
from pathlib import Path
import re
import runpy
import select
import signal
import socket
import struct
import subprocess
import tempfile
import time


class RtpUnits:
    """Strict local H.264/RTP accounting, not a decoder or jitter buffer.

    Require AUD (production aud=1), VCL, contiguous sequence numbers and closed
    FU-A at the marker. Never count a marker alone. Reordering is reported and
    conservatively rejected, not repaired. Retain only the first key AU as an
    optional offline ffprobe sample; timed runs perform no decoding or file IO.
    """
    def __init__(self):
        self.errors = Counter()
        self.units = []
        self.anchors = {}
        self.seen = set()
        self.recent = deque()
        self.generation = self.next_sequence = self.timestamp = None
        self.closed = True
        self.sample = None

    def abandon(self):
        if not self.closed:
            self.errors['partial_units'] += 1
        self.closed = True

    def nal(self, data):
        kind = data[0] & 31
        if data[0] & 128 or not 1 <= kind <= 23:
            raise ValueError('invalid NAL')
        self.aud |= kind == 9
        self.vcl |= kind in (1, 5)
        self.key |= kind == 5
        if self.sample is None:
            self.data.extend(b'\0\0\0\1' + data)

    def consume(self, packet, now):
        try:
            if len(packet) < 12 or packet[0] >> 6 != 2 or packet[1] & 127 != 96:
                raise ValueError('RTP header')
            offset = 12 + 4 * (packet[0] & 15)
            if packet[0] & 16:
                if len(packet) < offset + 4:
                    raise ValueError('RTP extension')
                offset += 4 + 4 * struct.unpack_from('!H', packet, offset + 2)[0]
            end = len(packet)
            if packet[0] & 32:
                if not packet[-1] or packet[-1] > end - offset:
                    raise ValueError('RTP padding')
                end -= packet[-1]
            if offset >= end:
                raise ValueError('empty/truncated RTP')
            sequence, timestamp, generation = struct.unpack_from('!HII', packet, 2)
            if not generation:
                raise ValueError('zero generation')
        except (ValueError, struct.error):
            self.errors['invalid_packets'] += 1
            if not self.closed:
                self.bad = True
            return
        identity = (generation, sequence)
        if identity in self.seen:
            self.errors['duplicate_packets'] += 1
            return
        if self.generation is not None and generation < self.generation:
            self.errors['stale_packets'] += 1
            return
        gap = 0
        if generation != self.generation:
            self.abandon()
            self.generation, self.next_sequence, self.timestamp = generation, sequence, None
            self.anchors[generation] = timestamp
        else:
            gap = (sequence - self.next_sequence) & 65535
            if gap >= 32768:
                self.errors['reordered_packets'] += 1
                return
        self.seen.add(identity)
        self.recent.append(identity)
        if len(self.recent) > 1024:
            self.seen.remove(self.recent.popleft())
        self.next_sequence = (sequence + 1) & 65535
        self.errors['missing_packets'] += gap
        if timestamp != self.timestamp:
            if self.timestamp is not None and ((timestamp-self.timestamp) & 0xffffffff) >= 0x80000000:
                self.errors['regressed_timestamps'] += 1
                if not self.closed:
                    self.bad = True
                return
            self.abandon()
            self.timestamp = timestamp
            self.closed = self.aud = self.vcl = self.key = False
            self.bad, self.fragment, self.data = bool(gap), None, bytearray()
        elif self.closed:
            self.errors['packets_after_marker'] += 1
            return
        self.bad |= bool(gap)
        payload = packet[offset:end]
        try:
            if payload[0] & 128:
                raise ValueError('forbidden NAL bit')
            kind = payload[0] & 31
            if self.fragment is not None and kind != 28:
                raise ValueError('interleaved FU-A')
            if 1 <= kind <= 23:
                self.nal(payload)
            elif kind == 24:
                position = 1
                if len(payload) == 1:
                    raise ValueError('empty STAP-A')
                while position < len(payload):
                    length = struct.unpack_from('!H', payload, position)[0]
                    position += 2
                    if not length or position + length > len(payload):
                        raise ValueError('STAP-A length')
                    self.nal(payload[position:position+length])
                    position += length
            elif kind == 28:
                if len(payload) < 3 or payload[1] & 32:
                    raise ValueError('FU-A header')
                start, finish = payload[1] & 128, payload[1] & 64
                header = (payload[0] & 224) | (payload[1] & 31)
                if start and not finish and self.fragment is None:
                    self.fragment = header
                    self.nal(bytes([header]) + payload[2:])
                elif not start and self.fragment == header:
                    if self.sample is None:
                        self.data.extend(payload[2:])
                    if finish:
                        self.fragment = None
                else:
                    raise ValueError('FU-A sequence')
            else:
                raise ValueError('unsupported packetization')
        except (ValueError, struct.error):
            self.errors['invalid_packets'] += 1
            self.bad = True
        if packet[1] & 128:
            if self.bad or self.fragment is not None or not self.aud or not self.vcl:
                self.errors['partial_units'] += 1
            else:
                self.units.append((generation, timestamp, now, self.key))
                if self.key and self.sample is None:
                    self.sample = bytes(self.data)
            self.closed = True


def distribution(values):
    values = sorted(values)
    return {name: round(values[int((len(values)-1)*fraction)], 3) if values else None
            for name, fraction in [('min', 0), ('p50', .5), ('p95', .95), ('p99', .99), ('max', 1)]}


def intervals(times):
    return distribution([(b-a)*1000 for a, b in zip(times, times[1:])])


def memory(pid):
    status = Path(f'/proc/{pid}/status').read_text()
    return {key: round(int(re.search(rf'{field}:\s+(\d+)', status)[1])/1024, 2)
            for key, field in [('rss_mib', 'VmRSS'), ('peak_rss_mib', 'VmHWM')]}


def cpu_seconds(pid):
    fields = Path(f"/proc/{pid}/stat").read_text().rsplit(") ", 1)[1].split()
    return (int(fields[11]) + int(fields[12])) / os.sysconf("SC_CLK_TCK")


def encoder_pid(pid):
    for task in Path(f"/proc/{pid}/task").iterdir():
        for child in (task / "children").read_text().split():
            if Path(f"/proc/{child}/comm").read_text().strip() == "ffmpeg":
                return int(child)
    raise RuntimeError("FFmpeg did not start")


def frame_metrics(frames, rtp, start, end, fps):
    """Two independent windows: capture cohort and local RTP completion arrival.

    RTP timestamps index FFmpeg's accepted raw frames, not capture sequence IDs.
    Join by generation and timestamp ordinal only after the final drain. Exact
    rendered attempts are unavailable: sequence gaps establish attempts and
    replacements only BETWEEN the first/last observed Submitted captures.
    """
    by_generation = defaultdict(list)
    for frame in frames:
        by_generation[frame['generation']].append(frame)
    cohort = [f for f in frames if start <= f['capture'] < end]
    completed = [u for u in rtp.units if start <= u[2] < end]
    # A missing leading AU makes ordinal zero ambiguous. Do not silently join
    # later deltas to the first Submitted record when the initial key was lost.
    anchored = {g for g, t, _, key in rtp.units if key and t == rtp.anchors[g]}
    matched, unmatched = {}, 0
    for generation, timestamp, arrived, _ in rtp.units:
        ordinal = ((timestamp - rtp.anchors[generation]) & 0xffffffff) * fps / 90000
        index = round(ordinal)
        records = by_generation[generation]
        if generation not in anchored or abs(index - ordinal) > .01 or index >= len(records):
            unmatched += 1
        else:
            frame = records[index]
            matched[(generation, frame['sequence'])] = arrived
    arrivals = [matched.get((f['generation'], f['sequence'])) for f in cohort]
    # Do not infer replacements across a generation transition.
    gaps = [b['sequence']-a['sequence']-1 for a, b in zip(cohort, cohort[1:])
            if a['generation'] == b['generation']]
    keys = [u for u in completed if u[3]]
    return {
        'submitted_frames': len(cohort), 'submitted_fps': round(len(cohort)/(end-start), 3),
        'submitted_receipts_in_window': sum(start <= f['received'] < end for f in frames),
        'complete_encoded_units': len(completed), 'complete_encoded_fps': round(len(completed)/(end-start), 3),
        'capture_cohort_completed_by_end': sum(a is not None and a < end for a in arrivals),
        'capture_cohort_completed_late': sum(a is not None and a >= end for a in arrivals),
        'capture_cohort_uncompleted_after_drain': sum(a is None for a in arrivals),
        'unmatched_complete_units': unmatched,
        'rendered_attempts_observed_span': len(cohort) + sum(gaps),
        'replaced_frames_observed_span': sum(gaps),
        'capture_span_seconds': cohort[-1]['capture']-cohort[0]['capture'] if len(cohort)>1 else 0,
        'capture_intervals_ms': intervals([f['capture'] for f in cohort]),
        'completion_intervals_ms': intervals([u[2] for u in completed]),
        'keyframe_completion_intervals_ms': intervals([u[2] for u in keys]),
        'keyframe_rtp_intervals_ms': distribution([
            ((b[1]-a[1]) & 0xffffffff)/90 for a, b in zip(keys, keys[1:]) if a[0] == b[0]]),
        'keyframes': len(keys),
    }


def measure(binary, receiver, directory, mode, args):
    width, height = map(int, args.resolution.split("x"))
    env = dict(os.environ, WAYWIRE_BENCH=mode, RUST_LOG="info")
    env.pop("WAYWIRE_SESSION", None)
    log = directory / "compositor.log"
    with socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as udp, log.open("wb") as errors:
        udp.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 4*1024*1024)
        udp.bind(("127.0.0.1", 0))
        process = subprocess.Popen([
            str(binary), "--rtp-port", str(udp.getsockname()[1]),
            "--resolution", args.resolution, "--frame-rate", str(args.fps),
            "--bitrate", str(args.bitrate),
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
            version = None
            frames = []
            acknowledged = set()
            rtp = RtpUnits()
            settings = set()
            packets = measured_packets = 0
            measured_bytes = 0
            start = time.monotonic() + args.warmup
            end = start + args.seconds
            cpu_start = cpu_end = None
            error_start = error_end = Counter()
            while time.monotonic() < end + args.drain:
                now = time.monotonic()
                if cpu_start is None and now >= start:
                    ffmpeg = encoder_pid(process.pid)
                    ffmpeg_args = Path(f'/proc/{ffmpeg}/cmdline').read_bytes().decode().split('\0')[:-1]
                    cpu_start = (now, cpu_seconds(process.pid), cpu_seconds(ffmpeg), time.process_time())
                    error_start = rtp.errors.copy()
                if cpu_end is None and now >= end:
                    cpu_end = (now, cpu_seconds(process.pid), cpu_seconds(ffmpeg), time.process_time())
                    memories = {'compositor': memory(process.pid), 'encoder': memory(ffmpeg)}
                    error_end = rtp.errors.copy()
                    # Freeze commits, without disconnect/unmap causing an extra repaint.
                    # Drain the existing pool/encoder while leaving FFmpeg stdin open.
                    client.send_signal(signal.SIGSTOP)
                if client.poll() is not None:
                    raise RuntimeError("benchmark client exited: " + log.read_text())
                boundary = start if cpu_start is None else end if cpu_end is None else end + args.drain
                for stream in select.select([process.stdout, udp], [], [], max(0, min(.02, boundary-now)))[0]:
                    if stream is udp:
                        packet = udp.recv(65536)
                        received = time.monotonic()
                        if start <= received < end:
                            measured_bytes += len(packet)
                            measured_packets += 1
                        rtp.consume(packet, received)
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
                        generation, encoded_width, encoded_height = struct.unpack_from("<IHH", payload)
                        captured, sequence = struct.unpack_from("<QQ", payload, 8)
                        actual_fps, chroma = struct.unpack_from('<IB', payload, len(payload)-5)
                        settings.add((encoded_width, encoded_height, actual_fps, chroma))
                        frames.append(dict(generation=generation, capture=captured/1e9,
                                           sequence=sequence, received=time.monotonic()))
                # Acknowledge only a validated key AU, not merely Submitted metadata.
                # Use the saved binary's wire version when comparing v8/v9 builds.
                if rtp.units and version is not None:
                    generation = rtp.units[-1][0]
                    if generation not in acknowledged and any(u[0] == generation and u[3] for u in rtp.units):
                        process.stdin.write(struct.pack("<BBHIIB", version, 11, 0, 5, generation, 1))
                        process.stdin.flush()
                        acknowledged.add(generation)
            rtp.abandon()
            elapsed = cpu_end[0] - cpu_start[0]
            compositor_cpu = cpu_end[1] - cpu_start[1]
            encoder_cpu = cpu_end[2] - cpu_start[2]
            metrics = frame_metrics(frames, rtp, start, end, args.fps)
            assert packets and acknowledged, "no encoded output"
            assert all(s[2] == args.fps for s in settings), "frame rate changed during run"
            assert mode == "idle" or metrics['submitted_frames'] > 1, "animation did not produce frames"
            assert mode != "idle" or not metrics['submitted_frames'], "idle output keeps encoding"
            if args.sample_dir:
                args.sample_dir.mkdir(parents=True, exist_ok=True)
                (args.sample_dir / f'{binary.name}-{mode}-{args.fps}.h264').write_bytes(rtp.sample)
            return {
                "binary": str(binary), "mode": mode, "resolution": args.resolution,
                "protocol_version": version,
                "target_fps": args.fps, 'bitrate_cap_kbps': args.bitrate,
                'actual_settings_width_height_fps_chroma': sorted(settings), 'ffmpeg_args': ffmpeg_args,
                "seconds": args.seconds, 'warmup_seconds': args.warmup, 'drain_seconds': args.drain,
                'window_monotonic_start': start, 'window_monotonic_end': end,
                **metrics,
                "rtp_kbps": round(measured_bytes * 8 / args.seconds / 1000, 2),
                'rtp_packets': measured_packets, 'rtp_receive_buffer_bytes': udp.getsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF),
                'rtp_errors_window': dict(error_end-error_start), 'rtp_errors_including_warmup_drain': dict(rtp.errors),
                'cpu_sample_seconds': elapsed,
                "compositor_cpu_percent": round(compositor_cpu / elapsed * 100, 2),
                "encoder_cpu_percent": round(encoder_cpu / elapsed * 100, 2),
                'receiver_cpu_percent': round((cpu_end[3]-cpu_start[3])/elapsed*100, 2),
                **memories,
            }
        finally:
            if client:
                client.send_signal(signal.SIGCONT)
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
    parser.add_argument("--rates", type=int, nargs='+', help="interleave rates; overrides --fps")
    parser.add_argument("--bitrate", type=int, default=16000, help="explicit encoder ceiling, Kbps")
    parser.add_argument("--seconds", type=float, default=6)
    parser.add_argument("--warmup", type=float, default=2)
    parser.add_argument("--drain", type=float, default=1, help="post-window seconds; excluded from rates/CPU")
    parser.add_argument("--sample-dir", type=Path, help="save first complete key AU per case for offline ffprobe")
    parser.add_argument("--repeat", type=int, default=3)
    parser.add_argument("--modes", nargs="+", choices=["full", "small", "idle"], default=["full", "small", "idle"])
    args = parser.parse_args()
    rates = args.rates or [args.fps]
    if args.seconds <= 0 or args.warmup <= 0 or args.drain <= 0 or args.repeat < 1:
        parser.error("seconds, warmup, drain, and repeat must be positive")
    if any(not 10 <= fps <= 120 for fps in rates) or not 300 <= args.bitrate <= 50000:
        parser.error("rates must be 10–120 FPS; bitrate must be 300–50000 Kbps")
    build_client = runpy.run_path(str(Path(__file__).with_name("native-scene.py")))["build_client"]
    with tempfile.TemporaryDirectory(prefix="waywire-bench-") as temporary:
        directory = Path(temporary)
        receiver = build_client(directory)
        binaries = [p.resolve(strict=True) for p in args.binaries]
        for run in range(args.repeat):
            for mode in args.modes:
                # Rotate which rate runs first to retain, rather than hide, host drift.
                for fps in rates[run % len(rates):] + rates[:run % len(rates)]:
                    args.fps = fps
                    for binary in binaries if run % 2 == 0 else reversed(binaries):
                        print(json.dumps({"run": run + 1, **measure(binary, receiver, directory, mode, args)}), flush=True)


if __name__ == "__main__":
    main()
