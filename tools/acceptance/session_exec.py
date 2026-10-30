#!/usr/bin/env python3
"""Execute a command with the environment exported by the session program."""

import os
import sys
from pathlib import Path

env = dict(
    entry.split("=", 1)
    for entry in Path("/tmp/session.env").read_bytes().decode().split("\0")
    if "=" in entry
)
env["CUA_DRIVER_RS_ENABLE_WAYLAND"] = "1"
os.execvpe(sys.argv[1], sys.argv[1:], env)
