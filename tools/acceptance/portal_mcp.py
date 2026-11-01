#!/usr/bin/env python3
"""Drive computer-use-linux over MCP in the compositor's session environment."""

import json
import select
import subprocess
import sys
from pathlib import Path

env = dict(
    item.split("=", 1)
    for item in Path("/tmp/session.env").read_bytes().decode().split("\0")
    if "=" in item
)
env.update(
    XDG_SESSION_TYPE="wayland",
    COMPUTER_USE_LINUX_FORCE_PORTAL_POINTER="1",
    COMPUTER_USE_LINUX_FORCE_PORTAL_KEYBOARD="1",
    COMPUTER_USE_LINUX_SCREENSHOT_BACKEND="portal",
    COMPUTER_USE_LINUX_INDICATOR="0",
)
process = subprocess.Popen(
    ["/opt/drivers/cul-stock", "mcp"],
    stdin=subprocess.PIPE,
    stdout=subprocess.PIPE,
    stderr=sys.stderr,
    env=env,
    text=True,
    bufsize=1,
)


def send(message):
    process.stdin.write(json.dumps(message) + "\n")
    process.stdin.flush()


def call(identifier, method, params):
    send({"jsonrpc": "2.0", "id": identifier, "method": method, "params": params})
    while True:
        if not select.select([process.stdout], [], [], 45)[0]:
            raise TimeoutError(f"MCP {method} did not respond")
        line = process.stdout.readline()
        if not line:
            raise RuntimeError("MCP server exited")
        result = json.loads(line)
        if result.get("id") == identifier:
            return result


call(
    1,
    "initialize",
    {
        "protocolVersion": "2024-11-05",
        "capabilities": {},
        "clientInfo": {"name": "waywire-acceptance", "version": "1"},
    },
)
send({"jsonrpc": "2.0", "method": "notifications/initialized"})
print(json.dumps({"ready": True}), flush=True)
try:
    for identifier, line in enumerate(sys.stdin, start=2):
        request = json.loads(line)
        result = call(identifier, "tools/call", request)
        if result.get("error") or result.get("result", {}).get("isError"):
            print(json.dumps(result), flush=True)
        else:
            print(json.dumps({"ok": True}), flush=True)
finally:
    process.terminate()
    process.wait(timeout=10)
