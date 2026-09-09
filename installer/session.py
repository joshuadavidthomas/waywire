#!/usr/bin/env python3
"""Own the compositor and gateway children for one desktop service."""

import contextlib
import os
from pathlib import Path
import signal
import subprocess
import sys
import time


_TERMINATION_GRACE_SECONDS = 8
_KILL_GRACE_SECONDS = 2


class OwnedProcess:
    """A child whose leader stays unreaped until its whole process group stops."""

    def __init__(self, process, descriptor):
        self._process = process
        self._descriptor = descriptor
        self.returncode = None
        self.stdout = process.stdout

    def poll(self):
        if self.returncode is not None:
            return self.returncode
        status = os.waitid(
            os.P_PIDFD, self._descriptor,
            os.WEXITED | os.WNOHANG | os.WNOWAIT,
        )
        if status is None:
            return None
        if status.si_code == os.CLD_EXITED:
            self.returncode = status.si_status
        else:
            self.returncode = -status.si_status
        return self.returncode


def _group_has_running_members(group):
    for entry in Path("/proc").iterdir():
        if not entry.name.isdigit():
            continue
        try:
            # The comm field may contain spaces and parentheses; all fields used
            # here follow its final closing parenthesis.
            fields = (entry / "stat").read_text(errors="replace").rsplit(")", 1)[1].split()
        except (FileNotFoundError, PermissionError, ProcessLookupError):
            continue
        try:
            state, process_group = fields[0], int(fields[2])
        except (IndexError, ValueError):
            continue
        if process_group == group and state != "Z":
            return True
    return False


def _signal_group(group, number):
    try:
        os.killpg(group, number)
    except ProcessLookupError:
        pass


@contextlib.contextmanager
def child(args, **kwargs):
    # A new session gives this child a private process group. The group leader is
    # deliberately not reaped until cleanup ends, reserving both its PID and PGID.
    kwargs["start_new_session"] = True
    process = subprocess.Popen(args, **kwargs)
    try:
        descriptor = os.pidfd_open(process.pid)
    except BaseException:
        _signal_group(process.pid, signal.SIGKILL)
        process.wait()
        raise
    owned = OwnedProcess(process, descriptor)
    try:
        yield owned
    finally:
        signal.signal(signal.SIGTERM, signal.SIG_IGN)
        signal.signal(signal.SIGINT, signal.SIG_IGN)
        _signal_group(process.pid, signal.SIGTERM)
        deadline = time.monotonic() + _TERMINATION_GRACE_SECONDS
        while _group_has_running_members(process.pid) and time.monotonic() < deadline:
            time.sleep(0.05)
        if _group_has_running_members(process.pid):
            _signal_group(process.pid, signal.SIGKILL)
            deadline = time.monotonic() + _KILL_GRACE_SECONDS
            while _group_has_running_members(process.pid) and time.monotonic() < deadline:
                time.sleep(0.01)
        group_still_running = _group_has_running_members(process.pid)
        try:
            owned.returncode = process.wait(timeout=_KILL_GRACE_SECONDS)
        finally:
            os.close(descriptor)
        if group_still_running:
            raise RuntimeError("child process group did not stop after SIGKILL")


def stop(_signal, _frame):
    raise SystemExit(1)


def main():
    signal.signal(signal.SIGTERM, stop)
    signal.signal(signal.SIGINT, stop)
    runtime = Path(os.environ["XDG_RUNTIME_DIR"])
    # Use the immutable release path in child argv, including across upgrades.
    root = Path("/opt/sprite-desktop/current/bin").resolve(strict=True)
    with (runtime / "labwc.log").open("w") as log, contextlib.ExitStack() as owned:
        compositor = owned.enter_context(child(
            ["labwc", "-C", str(Path(os.environ["XDG_CONFIG_HOME"]) / "labwc"),
             "-S", "lxqt-session"], stdout=log, stderr=subprocess.STDOUT,
        ))
        deadline = time.monotonic() + 15
        while True:
            if compositor.poll() is not None:
                raise RuntimeError("labwc exited; see " + str(runtime / "labwc.log"))
            try:
                ready = subprocess.run(
                    ["wayland-info"], stdout=subprocess.DEVNULL,
                    stderr=subprocess.DEVNULL, timeout=1, check=False,
                ).returncode == 0
            except subprocess.TimeoutExpired:
                ready = False
            if ready:
                break
            if time.monotonic() >= deadline:
                raise RuntimeError("labwc did not open its Wayland display within 15 seconds")
            time.sleep(0.1)
        subprocess.run(
            ["wlr-randr", "--output", "HEADLESS-1", "--custom-mode", "1280x720@60Hz"],
            check=True, timeout=5,
        )
        gateway = owned.enter_context(child([
            str(root / "sprite-desktop-gateway"), "--listen", "0.0.0.0:8080",
            "--streamd", str(root / "sprite-desktop-streamd"),
            "--public-url", sys.argv[1], "--frame-rate", "60", "--bitrate", "16000",
            "--xkb-layout", os.environ.get("XKB_DEFAULT_LAYOUT", "us"),
        ]))
        try:
            while compositor.poll() is None and gateway.poll() is None:
                time.sleep(0.1)
        finally:
            # A second termination request must not interrupt bounded cleanup.
            signal.signal(signal.SIGTERM, signal.SIG_IGN)
            signal.signal(signal.SIGINT, signal.SIG_IGN)
        raise RuntimeError("a desktop child exited; stopping the service")


if __name__ == "__main__":
    main()
