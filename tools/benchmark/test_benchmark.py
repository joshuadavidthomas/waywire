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

from benchmark import ENCODERS, main, process_snapshot


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
