#!/usr/bin/env python3
"""Exercise the pinned upstream ComputerTool using grim for screenshots."""

import asyncio
import os
import shlex
import sys

os.environ["WIDTH"] = "1280"
os.environ["HEIGHT"] = "720"

from computer_use_demo.tools.computer import ComputerTool20241022


class GrimComputerTool(ComputerTool20241022):
    _scaling_enabled = False
    _screenshot_delay = 0

    async def _capture(self, path):
        return await self.shell(f"grim {shlex.quote(str(path))}", take_screenshot=False)


async def main():
    tool = GrimComputerTool()
    for action, text in (
        ("key", "ctrl+u"),
        ("type", f"printf anthropic > {shlex.quote(sys.argv[1])}"),
        ("key", "Return"),
    ):
        result = await tool(action=action, text=text)
        if result.error:
            raise RuntimeError(result.error)
        if not result.base64_image:
            raise RuntimeError(f"{action} did not return a grim screenshot")


asyncio.run(main())
