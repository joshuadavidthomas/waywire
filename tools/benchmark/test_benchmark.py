"""Regression limits must reject slow, missing and incompatible results."""

import copy
import io
import sys
import unittest
from contextlib import redirect_stdout
from unittest.mock import patch

from benchmark import main


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


class RegressionCheckTest(unittest.TestCase):
    def setUp(self):
        self.results = {"waywire": result(), "waymote": result()}

    def run_check(self):
        output = io.StringIO()
        with (
            patch.object(sys, "argv", ["benchmark.py", "--check"]),
            patch("benchmark.subprocess.run"),
            patch("benchmark.browser"),
            patch(
                "benchmark.run_desktop", side_effect=lambda name, _: self.results[name]
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


if __name__ == "__main__":
    unittest.main()
