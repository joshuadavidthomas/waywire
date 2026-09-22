#!/usr/bin/env -S uv run --script
# /// script
# requires-python = ">=3.11"
# dependencies = ["aiohttp==3.12.15"]
# ///
"""Opt-in measurement sender. --prepare builds everything; serving never builds."""
import argparse
import asyncio
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import re
import signal
import socket
import subprocess

from aiohttp import ClientSession, WSMsgType, web

ROOT = Path(__file__).resolve().parents[4]
BUILD = Path(os.environ.get("LATENCY_BUILD", "/tmp/waywire-input-latency"))
HERE = Path(__file__).parent


def prepare():
    BUILD.mkdir(parents=True, exist_ok=True)
    subprocess.run(["just", "build-web"], cwd=ROOT, check=True)
    subprocess.run(["cargo", "build", "--locked", "--release", "--workspace"], cwd=ROOT, check=True)
    spec = importlib.util.spec_from_file_location("scene", HERE.parent / "native-scene.py")
    scene = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(scene)
    scene.build_client(BUILD)
    subprocess.run(["pnpm", "--filter", "@waywire/web", "exec", "vite", "build", str(HERE), "--outDir", str(BUILD / "web")], cwd=ROOT, check=True)
    metadata = {
        "cpu": subprocess.check_output(["lscpu"], text=True),
        "ffmpeg": subprocess.check_output(["ffmpeg", "-version"], text=True),
        "commit": subprocess.check_output(["git", "rev-parse", "HEAD"], cwd=ROOT, text=True).strip(),
        "hashes": {str(p): hashlib.sha256(p.read_bytes()).hexdigest() for p in [BUILD / "receiver", ROOT / "target/release/waywire-gateway", ROOT / "target/release/waywire-compositor"]},
    }
    (BUILD / "sender.json").write_text(json.dumps(metadata, indent=2))


class Sender:
    def __init__(self):
        self.process = None
        self.port = None
        self.lock = asyncio.Lock()
        self.serial = 0
        self.video_jitter = False
        self.relay_stalls = []
        self.started = 0

    def encoder_command(self):
        pending = [self.process.pid] if self.process else []
        while pending:
            pid = pending.pop()
            try:
                for children in Path(f"/proc/{pid}/task").glob("*/children"):
                    pending.extend(map(int, children.read_text().split()))
                args = Path(f"/proc/{pid}/cmdline").read_bytes().decode().strip("\0").split("\0")
                if args and Path(args[0]).name == "ffmpeg":
                    return args
            except FileNotFoundError:
                pass
        return None

    async def stop(self):
        if self.process and self.process.returncode is None:
            self.process.send_signal(signal.SIGTERM)
            await asyncio.wait_for(self.process.wait(), 15)
        self.process = None

    async def condition(self, request):
        values = await request.json()
        fps, mode = values.get("fps"), values.get("mode")
        if fps not in [60, 90, 120] or mode not in ["quiet", "motion"]:
            raise web.HTTPBadRequest()
        if not isinstance(values.get("videoJitter", False), bool):
            raise web.HTTPBadRequest()
        async with self.lock:
            await self.stop()
            self.video_jitter = values.get("videoJitter", False)
            self.relay_stalls = []
            self.started = asyncio.get_running_loop().time()
            with socket.socket() as sock:
                sock.bind(("127.0.0.1", 0))
                self.port = sock.getsockname()[1]
            self.serial += 1
            command = [str(ROOT / "target/release/waywire-gateway"), "--listen", f"127.0.0.1:{self.port}", "--compositor", str(ROOT / "target/release/waywire-compositor"), "--frame-rate", str(fps), "--bitrate", "16000", "--resolution", "1920x1080", "--", str(BUILD / "receiver"), "1920", "1080"]
            env = dict(os.environ, WAYWIRE_LATENCY=mode)
            env.pop("WAYWIRE_BENCH", None)
            env.pop("WAYWIRE_TEST_ACTIONS", None)
            env["PATH"] = str(Path.home() / ".local/share/waywire-xwayland/bin") + ":" + env["PATH"]
            with (BUILD / f"sender-{self.serial}.log").open("wb") as log:
                self.process = await asyncio.create_subprocess_exec(*command, cwd=ROOT, env=env, stdout=log, stderr=log)
            async with ClientSession() as client:
                for _ in range(200):
                    if self.process.returncode is not None:
                        raise web.HTTPInternalServerError(text="Sender exited; inspect log")
                    try:
                        async with client.get(f"http://127.0.0.1:{self.port}/healthz") as response:
                            if response.status == 200:
                                return web.json_response({"serial": self.serial, "fps": fps, "mode": mode, "videoJitter": self.video_jitter, "command": command, "encoder": self.encoder_command(), "sender": json.loads((BUILD / "sender.json").read_text())})
                    except OSError:
                        pass
                    await asyncio.sleep(.1)
            raise web.HTTPGatewayTimeout()

    async def proxy(self, request):
        if self.process is None:
            raise web.HTTPServiceUnavailable()
        async with ClientSession() as client:
            async with client.ws_connect(f"http://127.0.0.1:{self.port}{request.path_qs}", origin=request.headers.get("Origin"), max_msg_size=32 << 20) as upstream:
                downstream = web.WebSocketResponse(max_msg_size=32 << 20)
                await downstream.prepare(request)
                stalls, started = self.relay_stalls, self.started

                async def relay(source, destination, jitter=False):
                    loop = asyncio.get_running_loop()
                    next_stall = loop.time() + 2
                    async for message in source:
                        # A bounded video-only head-of-line pause, not a physical
                        # network model. Control/pongs and reverse traffic bypass it.
                        if jitter and loop.time() >= next_stall:
                            begin = loop.time()
                            await asyncio.sleep(.09)
                            stalls.append({"senderRelativeSeconds": begin - started, "durationMs": (loop.time() - begin) * 1000})
                            next_stall = begin + 2
                        if message.type == WSMsgType.BINARY:
                            await destination.send_bytes(message.data)
                        elif message.type == WSMsgType.TEXT:
                            await destination.send_str(message.data)
                    await destination.close()

                await asyncio.gather(relay(upstream, downstream, self.video_jitter and request.path == "/stream"), relay(downstream, upstream))
                return downstream


async def serve():
    sender = Sender()
    app = web.Application(client_max_size=64 << 20)
    app.router.add_post("/api/condition", sender.condition)
    app.router.add_get("/control", sender.proxy)
    app.router.add_get("/stream", sender.proxy)
    app.router.add_get("/healthz", lambda _: web.Response(text="ok"))
    app.router.add_get("/", lambda _: web.FileResponse(BUILD / "web/index.html"))
    app.router.add_static("/assets/", BUILD / "web/assets")

    async def save(request):
        name = request.query.get("name", "results")
        if not re.fullmatch(r"[a-zA-Z0-9_-]{1,80}", name):
            raise web.HTTPBadRequest(text="Invalid result name")
        data = await request.json()
        details = {"encoderAtSave": sender.encoder_command(), "relayStalls": sender.relay_stalls}
        if data.get("blocks"):
            data["blocks"][-1].update(details)
        path = BUILD / f"{name}.json"
        path.write_text(json.dumps(data))
        return web.json_response({"saved": str(path), "bytes": path.stat().st_size, "details": details})

    app.router.add_post("/api/results", save)

    async def stop(_):
        async with sender.lock:
            await sender.stop()
        return web.json_response({"stopped": True})

    app.router.add_post("/api/stop", stop)

    async def cleanup(_):
        await sender.stop()

    app.on_cleanup.append(cleanup)
    runner = web.AppRunner(app)
    await runner.setup()
    await web.TCPSite(runner, "127.0.0.1", int(os.environ["PORT"])).start()
    try:
        await asyncio.Event().wait()
    finally:
        await runner.cleanup()


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--prepare", action="store_true")
    if parser.parse_args().prepare:
        prepare()
    else:
        asyncio.run(serve())
