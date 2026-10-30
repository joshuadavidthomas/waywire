#!/usr/bin/env python3
"""Run one upstream ComputerTool action in the compositor session."""

import asyncio
import os
import sys
from pathlib import Path

for entry in Path("/tmp/session.env").read_bytes().split(b"\0"):
    if b"=" in entry:
        key, value = entry.split(b"=", 1)
        os.environ[os.fsdecode(key)] = os.fsdecode(value)
os.environ["WIDTH"] = "1280"
os.environ["HEIGHT"] = "720"

from computer_use_demo.tools.computer import ComputerTool20241022


class GrimComputerTool(ComputerTool20241022):
    _scaling_enabled = False
    _screenshot_delay = 0

    async def _capture(self, path):
        return await self.shell(f"grim {path}", take_screenshot=False)


async def main():
    result = await GrimComputerTool()(action=sys.argv[1], text=sys.argv[2])
    if result.error:
        raise RuntimeError(result.error)


asyncio.run(main())
