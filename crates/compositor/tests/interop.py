#!/usr/bin/env python3
"""Real XTest/reis -> native Wayland seat integration; requires CPU EI Xwayland.

Run after cargo build -p waywire-compositor --examples:
PATH="$HOME/.local/share/waywire-xwayland/bin:$PATH" python3 crates/compositor/tests/interop.py
"""
import os
from pathlib import Path
import re
import select
import shlex
import struct
import subprocess
import tempfile
import threading
import time

ROOT = Path(__file__).resolve().parents[3]


def wait_for(test, message, timeout=12):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if test():
            return
        time.sleep(0.025)
    raise AssertionError(message)


def run():
    with tempfile.TemporaryDirectory(prefix="waywire-interop-") as directory:
        work = Path(directory)
        xml = "/usr/share/wayland-protocols/stable/xdg-shell/xdg-shell.xml"
        for mode, name in [("client-header", "xdg-shell-client-protocol.h"), ("private-code", "xdg-shell-protocol.c")]:
            subprocess.run(["wayland-scanner", mode, xml, str(work / name)], check=True)
        subprocess.run(["cc", "-Wall", "-Wextra", "-Werror", str(Path(__file__).with_name("wayland-receiver.c")),
                        str(work / "xdg-shell-protocol.c"), f"-I{work}", "-lwayland-client", "-o", str(work / "receiver")], check=True)
        subprocess.run(["cc", "-Wall", "-Wextra", "-Werror", str(Path(__file__).with_name("xtest-driver.c")),
                        "-lX11", "-lXtst", "-o", str(work / "xtest-driver")], check=True)
        log = work / "receiver.log"
        environment = work / "environment"
        script = f'printf "%s\\n" "$WAYLAND_DISPLAY" "$DISPLAY" "$LIBEI_SOCKET" > {shlex.quote(str(environment))}; export WAYWIRE_TEST_ACTIONS=1; exec {shlex.quote(str(work / "receiver"))} 480 320 > {shlex.quote(str(log))}'
        errors = open(work / "compositor.log", "w+")
        compositor = subprocess.Popen([str(ROOT / "target/debug/waywire-compositor"), "--rtp-port", "15980", "--resolution", "640x480", "--frame-rate", "10", "--", "/bin/sh", "-c", script], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=errors)
        events = []

        def drain():
            while True:
                header = compositor.stdout.read(8)
                if len(header) != 8:
                    return
                version, kind, _, size = struct.unpack("<BBHI", header)
                events.append((version, kind, compositor.stdout.read(size)))

        threading.Thread(target=drain, daemon=True).start()
        children = []
        sequence = 0

        def command(kind, payload):
            compositor.stdin.write(struct.pack("<BBHI", 8, kind, 0, len(payload)) + payload)
            compositor.stdin.flush()

        def browser_key(code, down):
            nonlocal sequence
            sequence += 1
            command(4, struct.pack("<IBI", code, down, sequence))

        def browser_button(code, down):
            nonlocal sequence
            sequence += 1
            command(2, struct.pack("<IBI", code, down, sequence))

        def lines():
            return log.read_text().splitlines() if log.exists() else []

        def count(value):
            return lines().count(value)

        def sender():
            process = subprocess.Popen([str(ROOT / "target/debug/examples/ei-sender"), env["LIBEI_SOCKET"]], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=errors, text=True)
            children.append(process)
            assert reply(process) == "ready"
            return process

        def reply(process):
            assert select.select([process.stdout], [], [], 10)[0], "EI sender timed out"
            return process.stdout.readline().strip()

        def send(process, value):
            process.stdin.write(value + "\n")
            process.stdin.flush()
            assert reply(process) == "sent " + value
            time.sleep(0.1)

        def capture(variable):
            if screenshot := os.environ.get(variable):
                sdp = work / "stream.sdp"
                sdp.write_text("v=0\no=- 0 0 IN IP4 127.0.0.1\ns=waywire\nc=IN IP4 127.0.0.1\nt=0 0\nm=video 15980 RTP/AVP 96\na=rtpmap:96 H264/90000\n")
                subprocess.run(["ffmpeg", "-loglevel", "error", "-protocol_whitelist", "file,udp,rtp", "-i", str(sdp), "-frames:v", "1", "-y", screenshot], check=True, timeout=15)

        try:
            wait_for(lambda: environment.exists() and log.exists() and "paint 480 320" in lines(), "native session did not start")
            wayland, display, eis = environment.read_text().splitlines()
            pids = Path(f"/proc/{compositor.pid}/task/{compositor.pid}/children").read_text().split()
            xwayland_pid = next(pid for pid in pids if Path(f"/proc/{pid}/comm").read_text().strip() == "Xwayland")
            env = {**os.environ, "WAYLAND_DISPLAY": wayland, "DISPLAY": display, "LIBEI_SOCKET": eis, "XDG_RUNTIME_DIR": str(Path(wayland).parent)}
            subprocess.run([str(work / "xtest-driver")], env=env, check=True, timeout=10)
            wait_for(lambda: "key 30 1" in lines() and "key 30 0" in lines(), "XTest did not reach native keyboard")
            assert "button 272 1" in lines() and "button 272 0" in lines()
            print("PASS XTest keyboard/button events reached native Wayland receiver")

            direct = sender()
            send(direct, "motion 173 119")
            wait_for(lambda: "motion 141.000 87.000" in lines(), "EI logical coordinates were incorrectly transformed")
            send(direct, "key 48 1")
            send(direct, "key 48 0")
            wait_for(lambda: "key 48 1" in lines() and "key 48 0" in lines(), "direct reis keyboard did not arrive")
            print("PASS direct reis keyboard and asymmetric logical pointer coordinates")

            # A removed keyboard must not release the browser's same held key,
            # nor release a different EI device's button.
            key_releases = count("key 42 0")
            button_releases = count("button 272 0")
            browser_key(42, 1)
            send(direct, "key 42 1")
            send(direct, "button 272 1")
            send(direct, "close-keyboard")
            assert count("key 42 0") == key_releases
            assert count("button 272 0") == button_releases
            browser_key(42, 0)
            wait_for(lambda: count("key 42 0") == key_releases + 1, "source removal lost browser key ownership")
            browser_button(272, 1)
            direct.stdin.close()
            direct.wait(timeout=5)
            time.sleep(0.2)
            assert count("button 272 0") == button_releases
            browser_button(272, 0)
            wait_for(lambda: count("button 272 0") == button_releases + 1, "EI disconnect lost browser button ownership")
            print("PASS per-device removal and disconnect preserve browser key/button holds")

            direct = sender()
            before = count("key 29 0")
            send(direct, "key 29 1")
            direct.stdin.close()
            direct.wait(timeout=5)
            wait_for(lambda: count("key 29 0") == before + 1, "EI disconnect left key held")
            print("PASS EI-only held key released on disconnect")
            wait_for(lambda: any(kind == 2 and struct.unpack_from("<I", payload, 24)[0] == sequence for _, kind, payload in events), "browser sequence was not acknowledged")
            assert all(struct.unpack_from("<I", payload, 24)[0] <= sequence for _, kind, payload in events if kind == 2)
            print("PASS EI events do not invent browser input sequence acknowledgments")

            direct = sender()
            direct.stdin.write("region\n")
            direct.stdin.flush()
            assert reply(direct) == "region 640 480 1"
            send(direct, "key 42 1")
            key_releases = count("key 42 0")
            button_releases = count("button 272 0")
            send(direct, "button 272 1")
            command(6, struct.pack("<IIHH", 1200, 900, 180, 1))
            send(direct, "refresh")
            direct.stdin.write("region\n")
            direct.stdin.flush()
            assert reply(direct) == "region 800 600 1.5"
            wait_for(lambda: count("button 272 0") == button_releases + 1, "replaced absolute device left button held")
            assert count("key 42 0") == key_releases, "resize released the unchanged keyboard device"
            motions = count("motion 141.000 87.000")
            send(direct, "motion 173 119")
            wait_for(lambda: count("motion 141.000 87.000") > motions, "fractional scale divided EI logical coordinates")
            send(direct, "key 42 0")
            command(6, struct.pack("<IIHH", 640, 480, 120, 2))
            send(direct, "refresh")
            direct.stdin.close()
            direct.wait(timeout=5)
            before = count("key 30 1")
            subprocess.run([str(work / "xtest-driver")], env=env, check=True, timeout=10)
            wait_for(lambda: count("key 30 1") > before, "existing Xwayland EI sender did not survive region changes")
            print("PASS live EI region/scale refresh, device-specific release, and Xwayland sender reuse")

            result = work / "xterm-input"
            xterm = subprocess.Popen(["xterm", "-title", "waywire-interop", "-e", "/bin/sh", "-c", f'read value; printf "%s" "$value" > {shlex.quote(str(result))}; sleep 30'], env=env, stderr=errors)
            children.append(xterm)
            window = None
            def find_window():
                nonlocal window
                found = subprocess.run(["xdotool", "search", "--onlyvisible", "--name", "^waywire-interop$"], env=env, capture_output=True, text=True)
                if found.returncode == 0:
                    window = found.stdout.splitlines()[0]
                    return True
                return False
            wait_for(find_window, "real X11 xterm did not map")
            subprocess.run(["xdotool", "windowactivate", "--sync", window], env=env, check=True, timeout=10)
            time.sleep(0.2)
            subprocess.run(["xdotool", "type", "--clearmodifiers", "x11-ok"], env=env, check=True, timeout=10)
            subprocess.run(["xdotool", "key", "Return"], env=env, check=True, timeout=10)
            wait_for(lambda: result.exists() and result.read_text() == "x11-ok", "real X11 app did not receive keys")
            print("PASS real X11 xterm maps, activates, and receives shared-seat input")

            def geometry():
                response = subprocess.check_output(["xdotool", "getwindowgeometry", "--shell", window], env=env, text=True)
                fields = dict(line.split("=", 1) for line in response.splitlines())
                return tuple(int(fields[key]) for key in ["X", "Y", "WIDTH", "HEIGHT"])

            original = geometry()
            subprocess.run(["wmctrl", "-ir", hex(int(window)), "-b", "add,maximized_vert,maximized_horz"], env=env, check=True)
            wait_for(lambda: geometry() == (0, 24, 640, 450), "X11 maximize did not reserve accessible decorations")
            capture("WAYWIRE_INTEROP_MAXIMIZED_SCREENSHOT")
            subprocess.run(["wmctrl", "-ir", hex(int(window)), "-b", "add,fullscreen"], env=env, check=True)
            wait_for(lambda: geometry() == (0, 0, 640, 480), "X11 fullscreen retained decoration inset")
            subprocess.run(["wmctrl", "-ir", hex(int(window)), "-b", "remove,fullscreen"], env=env, check=True)
            wait_for(lambda: geometry() == (0, 24, 640, 450), "X11 fullscreen did not restore maximized geometry")
            subprocess.run(["wmctrl", "-ir", hex(int(window)), "-b", "remove,maximized_vert,maximized_horz"], env=env, check=True)
            wait_for(lambda: geometry() == original, "X11 unmaximize lost floating geometry")
            # Reverse the nesting order and leave maximized mode while still
            # fullscreen: the floating restore must not become fullscreen size.
            for action in ["add,fullscreen", "add,maximized_vert,maximized_horz", "remove,maximized_vert,maximized_horz", "remove,fullscreen"]:
                subprocess.run(["wmctrl", "-ir", hex(int(window)), "-b", action], env=env, check=True)
                time.sleep(0.1)
            wait_for(lambda: geometry() == original, "X11 reverse fullscreen nesting lost floating geometry")
            print("PASS X11 maximize/fullscreen nesting restores original floating geometry")

            direct = sender()
            send(direct, f"motion {original[0] + 40} {original[1] + 30}")
            old_tree = subprocess.check_output(["xwininfo", "-root", "-tree"], env=env, text=True)
            old_windows = set(re.findall(r"^\s+(0x[0-9a-f]+) ", old_tree, re.M))
            browser_key(29, 1)
            send(direct, "button 272 1")
            tree = subprocess.check_output(["xwininfo", "-root", "-tree"], env=env, text=True)
            new_windows = set(re.findall(r"^\s+(0x[0-9a-f]+) ", tree, re.M)) - old_windows
            assert any("Override Redirect State: yes" in subprocess.check_output(["xwininfo", "-id", popup], env=env, text=True) for popup in new_windows), "Xterm did not map override-redirect popup"
            print("PASS real X11 override-redirect popup maps")
            capture("WAYWIRE_INTEROP_SCREENSHOT")
            send(direct, "motion 620 460")
            send(direct, "button 272 0")
            browser_key(29, 0)
            send(direct, f"motion {original[0] + original[2] - 12} {original[1] - 12}")
            send(direct, "button 272 1")
            send(direct, "button 272 0")
            xterm.wait(timeout=5)
            print("PASS real X11 window closes through compositor decoration")

            # Use wl_data_device with a focused native client, not a clipboard
            # manager protocol: the test does not require privileged data-control.
            subprocess.run([str(work / "xtest-driver")], env=env, check=True, timeout=10)
            browser_key(46, 1)
            browser_key(46, 0)
            wait_for(lambda: any(kind == 1 and payload == b"native clipboard asymmetric 73" for _, kind, payload in events), "native clipboard not received by compositor")
            pasted = subprocess.run(["xclip", "-selection", "clipboard", "-o"], env=env, capture_output=True, timeout=5)
            assert pasted.returncode == 0, pasted.stderr
            assert pasted.stdout == b"native clipboard asymmetric 73", pasted.stdout
            xcopy = subprocess.Popen(["xclip", "-selection", "clipboard", "-quiet"], env=env, stdin=subprocess.PIPE, stderr=errors)
            children.append(xcopy)
            xcopy.stdin.write(b"x11-to-native")
            xcopy.stdin.close()
            wait_for(lambda: any(kind == 1 and payload == b"x11-to-native" for _, kind, payload in events), "X11 clipboard not received by compositor")
            browser_key(47, 1)
            browser_key(47, 0)
            wait_for(lambda: "clipboard x11-to-native" in lines(), "X11 clipboard not delivered to native wl_data_device")
            print("PASS native/X11 clipboard in both directions")
        except BaseException:
            errors.flush()
            print((work / "compositor.log").read_text())
            print("RECEIVER:\n" + "\n".join(lines()))
            raise
        finally:
            for child in children:
                if child.poll() is None:
                    child.terminate()
                    try:
                        child.wait(timeout=3)
                    except subprocess.TimeoutExpired:
                        child.kill()
                        child.wait()
            if compositor.poll() is None:
                compositor.stdin.close()
                try:
                    compositor.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    compositor.kill()
                    compositor.wait()
            errors.close()
        assert compositor.returncode == 0, f"compositor shutdown failed: {compositor.returncode}"
        wait_for(lambda: not Path(f"/proc/{xwayland_pid}/exe").exists(), "Xwayland survived compositor EOF shutdown")
        print("PASS compositor EOF shutdown leaves no Xwayland process")


def startup_failure():
    with tempfile.TemporaryDirectory(prefix="waywire-startup-") as directory:
        work = Path(directory)
        fake = work / "Xwayland"
        fake.write_text("#!/bin/sh\nexit 1\n")
        fake.chmod(0o755)
        marker = work / "session-started"
        process = subprocess.Popen([str(ROOT / "target/debug/waywire-compositor"), "--rtp-port", "15982", "--", "/usr/bin/touch", str(marker)], env={**os.environ, "PATH": f"{work}:{os.environ['PATH']}"}, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        try:
            assert process.wait(timeout=5) != 0
            assert not marker.exists(), "session started before Xwayland was ready"
        finally:
            if process.poll() is None:
                process.kill()
                process.wait()
            process.stdin.close()
        print("PASS Xwayland startup EOF fails promptly without launching session")


if __name__ == "__main__":
    run()
    startup_failure()
