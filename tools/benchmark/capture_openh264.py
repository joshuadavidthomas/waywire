#!/usr/bin/python3
"""Record raw desktop pictures for offline codec comparisons, without H.264.

Run with system Python and GStreamer/PipeWire GI bindings installed:
    /usr/bin/python3 tools/benchmark/capture_openh264.py /tmp/codec-corpus
Captures 90 BGRA frames per workload through the real ScreenCast portal.
No benchmark workload or acceptance threshold is changed.
"""

import hashlib
import json
import os
import shlex
import subprocess
import sys
import time
from pathlib import Path

import gi

gi.require_version("Gst", "1.0")
import benchmark
from gi.repository import Gio, GLib, Gst


def main():
    directory = Path(sys.argv[1]).resolve()
    directory.mkdir(parents=True, exist_ok=False)
    os.chmod(directory, 0o700)
    env = benchmark.desktop_env(str(directory))
    session_env = directory / "session.env"
    processes = []
    pipeline = None
    fd = None
    manifest = {"width": 1280, "height": 720, "fps": 30, "clips": {}}
    with (directory / "desktop.log").open("w") as log:
        try:
            command = benchmark.waywire_command(
                benchmark.URL,
                f"env -0 > {shlex.quote(str(session_env))}; exec xterm",
            )
            processes.append(benchmark.start(command, env, log))
            benchmark.wait_for(
                lambda: session_env.exists() and session_env.stat().st_size > 0,
                "desktop did not start",
            )
            app_env = dict(
                part.split("=", 1)
                for part in session_env.read_bytes().decode().split("\0")
                if "=" in part
            )
            # Connect explicitly, without changing the profiling process's bus.
            bus = Gio.DBusConnection.new_for_address_sync(
                app_env["DBUS_SESSION_BUS_ADDRESS"],
                Gio.DBusConnectionFlags.AUTHENTICATION_CLIENT
                | Gio.DBusConnectionFlags.MESSAGE_BUS_CONNECTION,
                None,
                None,
            )
            sender = bus.get_unique_name()[1:].replace(".", "_")
            portal = "org.freedesktop.portal.Desktop"
            path = "/org/freedesktop/portal/desktop"
            interface = "org.freedesktop.portal.ScreenCast"
            serial = 0

            def request(method, build):
                nonlocal serial
                serial += 1
                token = f"codec{os.getpid()}_{serial}"
                response = []
                subscription = bus.signal_subscribe(
                    portal,
                    "org.freedesktop.portal.Request",
                    "Response",
                    f"/org/freedesktop/portal/desktop/request/{sender}/{token}",
                    None,
                    Gio.DBusSignalFlags.NONE,
                    lambda *args: response.append(args[-1].unpack()),
                )
                try:
                    bus.call_sync(
                        portal,
                        path,
                        interface,
                        method,
                        build(token),
                        None,
                        0,
                        10000,
                        None,
                    )
                    context = GLib.MainContext.default()
                    deadline = time.monotonic() + 10
                    while not response and time.monotonic() < deadline:
                        context.iteration(False)
                        time.sleep(0.001)
                    if not response or response[0][0] != 0:
                        raise RuntimeError(f"{method}: {response}")
                    return response[0][1]
                finally:
                    bus.signal_unsubscribe(subscription)

            def options(token, **extra):
                return {"handle_token": GLib.Variant("s", token), **extra}

            session = request(
                "CreateSession",
                lambda t: GLib.Variant(
                    "(a{sv})",
                    (options(t, session_handle_token=GLib.Variant("s", "s" + t)),),
                ),
            )["session_handle"]
            request(
                "SelectSources",
                lambda t: GLib.Variant(
                    "(oa{sv})", (session, options(t, types=GLib.Variant("u", 1)))
                ),
            )
            node = request(
                "Start",
                lambda t: GLib.Variant("(osa{sv})", (session, "", options(t))),
            )["streams"][0][0]
            reply, fds = bus.call_with_unix_fd_list_sync(
                portal,
                path,
                interface,
                "OpenPipeWireRemote",
                GLib.Variant("(oa{sv})", (session, {})),
                GLib.VariantType.new("(h)"),
                0,
                10000,
                None,
                None,
            )
            fd = fds.get(reply.unpack()[0])
            Gst.init(None)
            pipeline = Gst.parse_launch(
                f"pipewiresrc name=source path={node} ! video/x-raw,format=BGRA ! "
                "appsink name=sink sync=false max-buffers=2 drop=true"
            )
            pipeline.get_by_name("source").set_property("fd", fd)
            pipeline.set_state(Gst.State.PLAYING)
            sink = pipeline.get_by_name("sink")

            def xdo(*args):
                return subprocess.check_output(
                    ["xdotool", *args], env=app_env, text=True
                )

            def capture(name, action=None):
                # Drain frames from the preceding workload before starting this one.
                while sink.emit("try-pull-sample", 0) is not None:
                    pass
                actor = None
                if action:
                    actor = benchmark.start(action, app_env, log)
                    processes.append(actor)
                hashes, timestamps, arrivals = [], [], []
                with (directory / f"{name}.bgra").open("wb") as output:
                    for _ in range(90):
                        sample = sink.emit("try-pull-sample", 5 * Gst.SECOND)
                        arrivals.append(time.monotonic_ns())
                        if sample is None:
                            message = pipeline.get_bus().pop_filtered(
                                Gst.MessageType.ERROR
                            )
                            raise RuntimeError(
                                message.parse_error() if message else "capture timeout"
                            )
                        caps = sample.get_caps().get_structure(0)
                        assert (caps.get_value("width"), caps.get_value("height")) == (
                            1280,
                            720,
                        )
                        buffer = sample.get_buffer()
                        data = buffer.extract_dup(0, buffer.get_size())
                        assert len(data) == 1280 * 720 * 4
                        output.write(data)
                        hashes.append(hashlib.sha256(data).hexdigest())
                        timestamps.append(buffer.pts)
                if actor:
                    benchmark.stop(actor)
                assert len(set(hashes)) > 10, f"{name} did not visibly change"
                manifest["clips"][name] = {
                    "frames": 90,
                    "hashes": hashes,
                    "pts_ns": timestamps,
                    "arrival_ns": arrivals,
                }
                print(
                    f"{name}: 90 frames, {len(set(hashes))} distinct pictures",
                    flush=True,
                )

            window = xdo(
                "search", "--sync", "--onlyvisible", "--class", "xterm"
            ).splitlines()[0]
            xdo("windowfocus", window)
            capture(
                "typing",
                ["xdotool", "type", "--delay", "50", "waywire benchmark typing " * 6],
            )
            page = directory / "scroll.html"
            page.write_text(
                "<!doctype html><title>Codec scroll</title>"
                + "".join(
                    f"<p>Line {i}: the quick brown fox jumps over the lazy dog.</p>"
                    for i in range(500)
                )
            )

            def chrome(page, title):
                process = benchmark.start(
                    [
                        "chromium",
                        "--no-sandbox",
                        "--disable-gpu",
                        "--disable-dev-shm-usage",
                        "--ozone-platform=x11",
                        f"--user-data-dir={directory / title}",
                        "--new-window",
                        page.as_uri(),
                    ],
                    app_env,
                    log,
                )
                processes.append(process)
                window = xdo(
                    "search", "--sync", "--onlyvisible", "--name", title
                ).splitlines()[0]
                xdo("windowfocus", window)
                time.sleep(1)
                return process

            browser = chrome(page, "Codec scroll")
            capture(
                "scroll", ["xdotool", "key", "--repeat", "80", "--delay", "100", "Down"]
            )
            benchmark.stop(browser)
            page = directory / "colors.html"
            page.write_text("""<!doctype html><title>Codec colors</title>
<style>body{margin:0;background:#aaa}canvas{display:block}</style><canvas width=1050 height=620></canvas>
<script>
const c=document.querySelector('canvas'), x=c.getContext('2d');let n=0;
function draw(){
 x.fillStyle='#ddd';x.fillRect(0,0,1050,620);
 x.font='16px monospace';
 for(let j=0;j<8;j++){
  x.fillStyle=['#222','#d0d0d0','#bcbcbc','#b0b0b0'][j%4];
  x.fillText('Low contrast text: Il1| O0 @ # + =; chroma edges',12,25+j*23);
 }
 // Similar-luma colors force chroma-sensitive quality checks.
 x.fillStyle=n%2?'rgb(180,60,80)':'rgb(60,90,140)';x.fillRect(16,230,360,180);
 for(let j=0;j<60;j++){
  x.fillStyle=`hsl(${j*6},70%,50%)`;x.fillRect(400+j*8,230,4,180);
 }
 x.fillStyle='#222';x.fillRect(20+(n*5)%850,460,100,60);
 n++;setTimeout(draw,33);
}draw();</script>""")
            chrome(page, "Codec colors")
            capture("colors")
            for name in ("waywire-compositor", "waywire-gateway"):
                with (benchmark.ROOT / "target/release" / name).open("rb") as binary:
                    manifest[name] = hashlib.file_digest(binary, "sha256").hexdigest()
            (directory / "manifest.json").write_text(json.dumps(manifest, indent=2))
        finally:
            if pipeline is not None:
                pipeline.set_state(Gst.State.NULL)
            if fd is not None:
                os.close(fd)
            for process in reversed(processes):
                benchmark.stop(process)
            session_env.unlink(missing_ok=True)


if __name__ == "__main__":
    main()
