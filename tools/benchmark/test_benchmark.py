"""Regression limits must reject slow, missing and incompatible results."""

import copy
import io
import os
import signal
import subprocess
import sys
import unittest
from contextlib import redirect_stdout
from unittest.mock import patch

from benchmark import ENCODERS, latency_breakdown, main, process_snapshot


def result():
    return {
        "launch_ms": 100,
        "idle": {"cpu_percent": 10, "rss_mib": 100},
        "typing": {
            "cpu_percent": 10,
            "rss_mib": 100,
            "kbps_by_socket": {"/stream": 100},
        },
        "scroll": {
            "cpu_percent": 10,
            "rss_mib": 100,
            "kbps_by_socket": {"/stream": 100},
        },
        "latency": {"median_ms": 100, "p95_ms": 100},
        "agents": {"xdotool": "pass", "anthropic_grim": "pass"},
    }


def slow_link_result():
    return {
        "lowest_scale": 50,
        "full_scale": 100,
        "lowest_bitrate": 6000,
        "full_bitrate": 12000,
        "slow_fps": 12.0,
        "slow_kbps": 480,
        "slow_age_ms": 3500,
        "recovery_s": 90.0,
        "samples": [],
    }


class ProcessSnapshotTest(unittest.TestCase):
    def test_session_members_are_included_without_ancestry(self):
        process = subprocess.Popen(
            ["sh", "-c", "(sleep 30 >/dev/null 2>&1 & echo $!); sleep 30"],
            start_new_session=True,
            stdout=subprocess.PIPE,
            text=True,
        )
        child = int(process.stdout.readline())
        try:
            self.assertNotIn(child, {pid for pid, _ in process_snapshot([process.pid])})
            self.assertIn(
                child,
                {pid for pid, _ in process_snapshot([process.pid], [process.pid])},
            )
        finally:
            os.killpg(process.pid, signal.SIGTERM)
            process.wait()
            process.stdout.close()


class LatencyBreakdownTest(unittest.TestCase):
    def test_stages_join_on_one_clock_and_add_up(self):
        # Server clock = browser clock + 1000 ms, measured by a 0.4 ms ping.
        clock = [
            {"sent": 0.0, "received": 2.0, "serverNanos": "1001000000"},
            {"sent": 5.0, "received": 5.4, "serverNanos": "1005200000"},
        ]

        def line(fields):
            return f"2026-11-01T00:00:00Z DEBUG waywire_latency: {fields}"

        def ns(browser_ms):
            return int((browser_ms + 1000) * 1e6)

        log = "\n".join(
            line(fields)
            for fields in (
                f'stage="render" sequence=6 start_ns={ns(8)} at_ns={ns(9)}',
                f'stage="gateway_input" input=3 at_ns={ns(11)}',
                f'stage="compositor_input" input=3 at_ns={ns(12)}',
                f'stage="render" sequence=7 start_ns={ns(13)} at_ns={ns(14)}',
                # The previous render started after this commit, so it is not the change.
                f'stage="commit" cursor=false at_ns={ns(12.5)}',
                f'stage="commit" cursor=true at_ns={ns(15)}',
                f'stage="commit" cursor=false at_ns={ns(16)}',
                f'stage="commit" cursor=false at_ns={ns(17)}',
                f'stage="render" sequence=8 start_ns={ns(20)} at_ns={ns(21)}',
                f'stage="encoder_input" sequence=8 start_ns={ns(22)} at_ns={ns(24)}',
                # The same RTP timestamp in an older generation must not match.
                f'stage="encoded" generation=1 rtp_timestamp=900 bytes=10 at_ns={ns(25)}',
                f'stage="encoded" generation=2 rtp_timestamp=900 bytes=10 at_ns={ns(30)}',
                f'stage="correlated" sequence=8 generation=2 rtp_timestamp=900 at_ns={ns(31)}',
                f'stage="gateway_send" sequence=8 start_ns={ns(31.5)} at_ns={ns(32)}',
            )
        )
        traces = [
            {
                "keydown": 10.0,
                "armed": 0.5,
                "sent": 10.8,
                "input": 3,
                "sequence": 8,
                "received": 33.0,
                "decoding": 33.5,
                "decoded": 35.0,
                "drawn": 60.0,
                "detected": 62.0,
            }
        ]
        breakdown = latency_breakdown(traces, clock, log)
        stages = {name: stage["p50_ms"] for name, stage in breakdown["stages"].items()}
        self.assertEqual(breakdown["clock_uncertainty_ms"], 0.2)
        self.assertEqual(
            stages,
            {
                "keydown → viewer handler (harness readback)": 0.5,
                "viewer handler → input sent": 0.3,
                "input sent → gateway receive": 0.2,
                "gateway → compositor input": 1.0,
                "compositor input → client commit": 4.0,
                "client commit → render start": 4.0,
                "render": 1.0,
                "render → encoder thread": 1.0,
                "raw frame pipe write": 2.0,
                "FFmpeg encode → RTP received": 6.0,
                "RTP received → frame correlated": 1.0,
                "correlated → gateway send done": 1.0,
                "gateway send → browser receive": 1.0,
                "browser receive → decode submit": 0.5,
                "decode": 1.5,
                "decoded → canvas paint": 25.0,
                "canvas paint → changed pixel (harness readback)": 2.0,
            },
        )
        self.assertEqual(breakdown["encoder"], "ffmpeg")
        self.assertEqual(breakdown["sum_of_stage_mean_ms"], 52.0)
        self.assertEqual(breakdown["end_to_end"]["mean_ms"], 52.0)

    def test_in_process_encoder_has_its_own_stages(self):
        clock = [{"sent": 0.0, "received": 0.4, "serverNanos": "1000200000"}]

        def ns(browser_ms):
            return int((browser_ms + 1000) * 1e6)

        log = "\n".join(
            f"2026-11-01T00:00:00Z DEBUG waywire_latency: {fields}"
            for fields in (
                f'stage="gateway_input" input=3 at_ns={ns(11)}',
                f'stage="compositor_input" input=3 at_ns={ns(12)}',
                f'stage="commit" cursor=false at_ns={ns(13)}',
                f'stage="render" sequence=8 start_ns={ns(14)} at_ns={ns(15)}',
                f'stage="converted" sequence=8 start_ns={ns(15)} at_ns={ns(15.5)}',
                f'stage="encoder_output" sequence=8 start_ns={ns(16)} at_ns={ns(18)}',
                f'stage="emitted" sequence=8 start_ns=0 at_ns={ns(18.5)}',
                f'stage="encoded_received" sequence=8 at_ns={ns(19)}',
                f'stage="gateway_send" sequence=8 start_ns={ns(19.5)} at_ns={ns(20)}',
            )
        )
        traces = [
            {
                "keydown": 10.0,
                "armed": 0.5,
                "sent": 10.8,
                "input": 3,
                "sequence": 8,
                "received": 21.0,
                "decoding": 21.5,
                "decoded": 22.0,
                "drawn": 25.0,
                "detected": 27.0,
            }
        ]
        breakdown = latency_breakdown(traces, clock, log)
        stages = {name: stage["p50_ms"] for name, stage in breakdown["stages"].items()}
        self.assertEqual(breakdown["encoder"], "openh264")
        self.assertEqual(
            list(stages.items())[7:13],
            [
                ("BGRA→I420 of damaged rows", 0.5),
                ("converted → encoder thread", 0.5),
                ("OpenH264 encode", 2.0),
                ("encoder → compositor event out", 0.5),
                ("compositor → gateway receive", 0.5),
                ("gateway receive → send done", 1.0),
            ],
        )
        self.assertEqual(breakdown["sum_of_stage_mean_ms"], 17.0)
        self.assertEqual(breakdown["end_to_end"]["mean_ms"], 17.0)


class RegressionCheckTest(unittest.TestCase):
    def setUp(self):
        self.results = {
            "waywire": result(),
            "waymote": result(),
            "slow_link": {encoder: slow_link_result() for encoder in ENCODERS},
        }

    def run_check(self):
        output = io.StringIO()
        with (
            patch.object(sys, "argv", ["benchmark.py", "--check"]),
            patch("benchmark.subprocess.run"),
            patch("benchmark.browser"),
            patch(
                "benchmark.run_desktop", side_effect=lambda name, _: self.results[name]
            ),
            patch(
                "benchmark.slow_link",
                side_effect=lambda encoder: self.results["slow_link"].get(encoder),
            ),
            redirect_stdout(output),
        ):
            status = main()
        return status, output.getvalue().strip()

    def test_tolerance_is_inclusive(self):
        self.results["waywire"]["typing"]["cpu_percent"] = 14
        self.assertEqual(self.run_check(), (0, "Regression check passed"))
        self.results["waywire"]["typing"]["cpu_percent"] = 14.1
        self.assertEqual(
            self.run_check(),
            (1, "Regression check failed:\ntyping CPU %: 14.1 > 14.0 (Waymote 10)"),
        )

    def test_missing_metric_fails(self):
        self.results["waywire"]["latency"] = {"status": "n/a"}
        self.assertEqual(
            self.run_check(),
            (
                1,
                "Regression check failed:\nlatency median_ms: unavailable\nlatency p95_ms: unavailable",
            ),
        )

    def test_agent_regression_fails(self):
        self.results["waywire"]["agents"]["anthropic_grim"] = "fail"
        self.assertEqual(
            self.run_check(),
            (
                1,
                "Regression check failed:\nanthropic_grim: waywire failed, Waymote passed",
            ),
        )

    def test_baseline_failure_is_not_a_regression(self):
        self.results["waymote"] = copy.deepcopy(self.results["waywire"])
        self.results["waymote"]["agents"]["xdotool"] = "fail"
        self.results["waywire"]["agents"]["xdotool"] = "fail"
        self.assertEqual(self.run_check(), (0, "Regression check passed"))

    def test_slow_link_must_adapt_scale_and_bitrate_not_only_frame_rate(self):
        only_fps = self.results["slow_link"]["openh264"]
        only_fps.update(lowest_scale=100, lowest_bitrate=12000)
        self.assertEqual(
            self.run_check(),
            (
                1,
                "Regression check failed:\nslow link openh264: scale stayed at 100"
                "\nslow link openh264: bitrate stayed at 12000",
            ),
        )

    def test_slow_link_frame_rate_floor_is_inclusive(self):
        self.results["slow_link"]["ffmpeg"]["slow_fps"] = 4
        self.assertEqual(self.run_check(), (0, "Regression check passed"))
        self.results["slow_link"]["ffmpeg"]["slow_fps"] = 3.9
        self.assertEqual(
            self.run_check(),
            (1, "Regression check failed:\nslow link ffmpeg fps: 3.9 < 4"),
        )

    def test_slow_link_frame_age_bound_is_inclusive(self):
        self.results["slow_link"]["openh264"]["slow_age_ms"] = 5000
        self.assertEqual(self.run_check(), (0, "Regression check passed"))
        # A backlog hidden in a kernel send buffer left frames seconds old.
        self.results["slow_link"]["openh264"]["slow_age_ms"] = 20036
        self.assertEqual(
            self.run_check(),
            (
                1,
                "Regression check failed:"
                "\nslow link openh264 frame age: 20036 ms > 5000 ms",
            ),
        )

    def test_slow_link_must_recover_and_be_measured(self):
        self.results["slow_link"]["ffmpeg"]["recovery_s"] = None
        del self.results["slow_link"]["openh264"]
        self.assertEqual(
            self.run_check(),
            (
                1,
                "Regression check failed:"
                "\nslow link ffmpeg: quality did not recover within 150 s"
                "\nslow link openh264: unavailable",
            ),
        )


if __name__ == "__main__":
    unittest.main()
