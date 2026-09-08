"""Start real native children for the disposable local fixture."""
import contextlib
import os
from pathlib import Path
import signal
import subprocess
import time

from session_owner import child, stop

signal.signal(signal.SIGTERM, stop)
signal.signal(signal.SIGINT, stop)
runtime = Path(os.environ["XDG_RUNTIME_DIR"])
with (runtime / "labwc.log").open("w") as log, contextlib.ExitStack() as owned:
    compositor = owned.enter_context(child(
        ["labwc", "-S", "/opt/socket-local/session.sh"],
        stdout=log, stderr=subprocess.STDOUT,
    ))
    ready = False
    for _ in range(100):
        if compositor.poll() is not None:
            raise RuntimeError("fixture compositor exited")
        if (runtime / "wayland-0").is_socket() and subprocess.run(
            ["wayland-info"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
            timeout=2, check=False,
        ).returncode == 0:
            ready = True
            break
        time.sleep(0.1)
    if not ready:
        raise RuntimeError("fixture compositor did not become ready")
    subprocess.run(["wlr-randr", "--output", "HEADLESS-1", "--custom-mode", "1280x720@60Hz"], check=True, timeout=5)
    gateway = owned.enter_context(child([
        "/opt/socket-local/bin/sprite-desktop-gateway", "--listen", "0.0.0.0:8080",
        "--streamd", "/opt/socket-local/bin/sprite-desktop-streamd",
        "--public-url", os.environ["PUBLIC_URL"], "--frame-rate", os.environ["FRAME_RATE"],
        "--bitrate", os.environ["BITRATE"], "--xkb-layout", "us",
    ]))
    while compositor.poll() is None and gateway.poll() is None:
        time.sleep(0.1)
    raise RuntimeError("fixture native child exited")
