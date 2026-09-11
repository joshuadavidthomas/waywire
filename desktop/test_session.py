from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time
import unittest
from unittest.mock import patch

from session import child


class OwnedChildTest(unittest.TestCase):
    def setUp(self):
        self.handlers = {s: signal.getsignal(s) for s in (signal.SIGTERM, signal.SIGINT)}

    def tearDown(self):
        for number, handler in self.handlers.items():
            signal.signal(number, handler)

    def test_live_child_is_stopped_and_reaped(self):
        with child([sys.executable, "-c", "import time; time.sleep(60)"]) as process:
            self.assertIsNone(process.poll())
        self.assertIsNotNone(process.returncode)

    def test_exited_child_is_observed_without_reaping_its_leader(self):
        with child([sys.executable, "-c", "pass"]) as process:
            deadline = time.monotonic() + 5
            while process.poll() is None and time.monotonic() < deadline:
                time.sleep(0.01)
            self.assertEqual(process.poll(), 0)
        self.assertEqual(process.returncode, 0)

    def test_uncooperative_grandchild_is_stopped(self):
        with tempfile.TemporaryDirectory() as directory:
            pid_file = Path(directory) / "pid"
            script = (
                "import pathlib,signal,subprocess,sys,time; "
                "child=subprocess.Popen([sys.executable,'-c',"
                "'import signal,time; signal.signal(signal.SIGTERM, signal.SIG_IGN); time.sleep(60)']); "
                "pathlib.Path(sys.argv[1]).write_text(str(child.pid)); time.sleep(0.2); time.sleep(60)"
            )
            with patch("session._TERMINATION_GRACE_SECONDS", 0.1):
                with child([sys.executable, "-c", script, str(pid_file)]):
                    deadline = time.monotonic() + 5
                    while not pid_file.exists() and time.monotonic() < deadline:
                        time.sleep(0.01)
                    grandchild = int(pid_file.read_text())
            self.assertFalse(self._running(grandchild))

    def test_exited_leader_keeps_group_owned_until_grandchild_cleanup(self):
        with tempfile.TemporaryDirectory() as directory:
            pid_file = Path(directory) / "pid"
            script = (
                "import pathlib,signal,subprocess,sys; "
                "child=subprocess.Popen([sys.executable,'-c',"
                "'import signal,time; signal.signal(signal.SIGTERM, signal.SIG_IGN); time.sleep(60)']); "
                "pathlib.Path(sys.argv[1]).write_text(str(child.pid)); import time; time.sleep(0.2)"
            )
            with patch("session._TERMINATION_GRACE_SECONDS", 0.1):
                with child([sys.executable, "-c", script, str(pid_file)]) as process:
                    deadline = time.monotonic() + 5
                    while process.poll() is None and time.monotonic() < deadline:
                        time.sleep(0.01)
                    self.assertEqual(process.poll(), 0)
                    grandchild = int(pid_file.read_text())
                    self.assertTrue(self._running(grandchild))
            self.assertFalse(self._running(grandchild))

    @staticmethod
    def _running(pid):
        try:
            state = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()[0]
        except FileNotFoundError:
            return False
        return state != "Z"

    def test_exception_cleans_up_child(self):
        with self.assertRaisesRegex(RuntimeError, "startup failed"):
            with child([sys.executable, "-c", "import time; time.sleep(60)"]) as process:
                raise RuntimeError("startup failed")
        self.assertIsNotNone(process.returncode)

    def test_cooperative_child_receives_term(self):
        with child([
            sys.executable, "-u", "-c",
            "import signal,time; signal.signal(signal.SIGTERM, lambda *_: exit(23)); "
            "print('ready', flush=True); time.sleep(60)",
        ], stdout=subprocess.PIPE, text=True) as process:
            self.assertEqual(process.stdout.readline(), "ready\n")
        self.assertEqual(process.returncode, 23)
        process.stdout.close()


if __name__ == "__main__":
    unittest.main()
