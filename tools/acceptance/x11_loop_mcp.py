#!/usr/bin/env python3
"""Stdio MCP server for the xdotool loop: Anthropic's ComputerTool20250124 at
anthropic-quickstarts 1a18c34, with grim capturing the Wayland output as in the
takeover scenario, plus a bash tool like the demo's.

It keeps the demo's 2 s post-action screenshot delay. Run it with the session's
environment (DISPLAY points at waywire's Xwayland).
"""

import asyncio
import json
import os
import subprocess
import sys

sys.path.insert(0, "/opt/anthropic-quickstarts/computer-use-demo")
os.environ.setdefault("WIDTH", "1280")
os.environ.setdefault("HEIGHT", "720")

from computer_use_demo.tools.base import ToolError  # noqa: E402
from computer_use_demo.tools.computer import ComputerTool20250124  # noqa: E402


class GrimComputerTool(ComputerTool20250124):
    async def _capture(self, path):
        return await self.shell(f"grim {path}", take_screenshot=False)


tool = GrimComputerTool()


def reply(identifier, result):
    sys.stdout.write(
        json.dumps({"jsonrpc": "2.0", "id": identifier, "result": result}) + "\n"
    )
    sys.stdout.flush()


def text(value, error=False):
    return {"content": [{"type": "text", "text": value}], "isError": error}


async def call(name, args):
    if name == "bash":
        try:
            process = subprocess.run(
                ["bash", "-c", args.get("command", "")],
                capture_output=True,
                text=True,
                timeout=60,
            )
        except subprocess.TimeoutExpired:
            return text("timed out", True)
        output = process.stdout + (
            "\nstderr: " + process.stderr if process.stderr else ""
        )
        return text(output[-8000:] or "(no output)", process.returncode != 0)
    try:
        result = await tool(**args)
    except ToolError as error:
        return text(str(error), True)
    content = []
    if result.output:
        content.append({"type": "text", "text": result.output})
    if result.error:
        content.append({"type": "text", "text": "stderr: " + result.error})
    if result.base64_image:
        content.append(
            {"type": "image", "data": result.base64_image, "mimeType": "image/png"}
        )
    return {"content": content or [{"type": "text", "text": "ok"}], "isError": False}


async def main():
    loop = asyncio.get_running_loop()
    while line := await loop.run_in_executor(None, sys.stdin.readline):
        message = json.loads(line)
        method, identifier = message.get("method"), message.get("id")
        if identifier is None:
            continue
        if method == "initialize":
            reply(
                identifier,
                {
                    "protocolVersion": message["params"]["protocolVersion"],
                    "capabilities": {"tools": {}},
                    "serverInfo": {"name": "x11-loop", "version": "1a18c34"},
                },
            )
        elif method == "tools/call":
            params = message["params"]
            reply(identifier, await call(params["name"], params.get("arguments", {})))
        else:
            reply(identifier, {})


asyncio.run(main())
