#!/usr/bin/env python3
"""Real wl_shm clients through the production compositor/encoder boundary.

Run after cargo build -p waywire-compositor. Needs cc, wayland-scanner,
wayland-client development files, and Xwayland on PATH. No DRM or display.
The encoder stand-in preserves complete raw frames, not fabricated protocol events.
"""
import glob
import os
from pathlib import Path
import re
import select
import struct
import subprocess
import sys
import tempfile
import time


def encoder():
    args = sys.argv
    width, height = map(int, args[args.index("-video_size") + 1].split("x"))
    generation = args[args.index("-ssrc") + 1]
    path = Path(os.environ["WAYWIRE_TEST_FRAMES"]) / f"{generation}-{width}x{height}.bgra"
    while True:
        frame = sys.stdin.buffer.read(width * height * 4)
        if len(frame) != width * height * 4:
            return
        temporary = path.with_suffix(".tmp")
        temporary.write_bytes(frame)
        temporary.replace(path)


def wait(predicate, label, timeout=8):
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        result = predicate()
        if result:
            return result
        time.sleep(0.02)
    raise AssertionError(f"timed out: {label}")


def build_client(directory):
    roots = [Path("/usr/share/wayland-protocols")]
    roots += [Path(p) / "protocols" for p in glob.glob(str(Path.home() / ".cargo/registry/src/*/wayland-protocols-*"))]
    sources = []
    for name, suffix in [
        ("xdg-shell", "stable/xdg-shell/xdg-shell.xml"),
        ("xdg-decoration", "unstable/xdg-decoration/xdg-decoration-unstable-v1.xml"),
        ("fractional-scale", "staging/fractional-scale/fractional-scale-v1.xml"),
        ("text-input", "unstable/text-input/text-input-unstable-v3.xml"),
    ]:
        xml = next(root / suffix for root in roots if (root / suffix).exists())
        source = directory / f"{name}.c"
        subprocess.run(["wayland-scanner", "client-header", str(xml), str(directory / f"{name}-client-protocol.h")], check=True)
        subprocess.run(["wayland-scanner", "private-code", str(xml), str(source)], check=True)
        sources.append(str(source))
    binary = directory / "receiver"
    flags = subprocess.check_output(["pkg-config", "--cflags", "--libs", "wayland-client"], text=True).split()
    subprocess.run(["cc", "-Wall", "-Wextra", "-Werror", "-DNATIVE_SCENE_TEST", str(Path(__file__).with_name("wayland-receiver.c")), *sources, "-I" + str(directory), *flags, "-o", str(binary)], check=True)
    return binary


class Scene:
    def __init__(self, directory):
        self.directory = directory
        self.receiver = build_client(directory)
        self.clients = []
        self.sequence = 0
        self.events = []
        self.pending = b""
        self.size = (800, 600)
        env = dict(os.environ, WAYWIRE_TEST_FRAMES=str(directory), RUST_LOG="info")
        env.pop("WAYWIRE_SESSION", None)
        self.log = directory / "compositor.log"
        self.process = subprocess.Popen([str(Path("target/debug/waywire-compositor").resolve()), "--rtp-port", "59999", "--resolution", "800x600", "--ffmpeg", str(Path(__file__).resolve())], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=self.log.open("wb"), env=env)
        try:
            socket = wait(lambda: re.search(r"socket=([^\s]+)", self.log.read_text()), "Wayland socket")
        except Exception:
            self.process.stdin.close()
            self.process.wait(timeout=8)
            print(self.log.read_text(), file=sys.stderr)
            raise
        self.env = dict(env, WAYLAND_DISPLAY=socket[1], WAYWIRE_TEST_ACTIONS="1", WAYWIRE_TEST_DECORATIONS="1")

    def close(self):
        for process, _ in self.clients:
            if process.poll() is None:
                process.terminate()
            process.wait(timeout=5)
        self.process.stdin.close()
        self.process.wait(timeout=8)
        assert self.process.returncode == 0, self.log.read_text()

    def client(self, width, height, **extra_env):
        path = self.directory / f"client-{len(self.clients)}.log"
        process = subprocess.Popen([str(self.receiver), str(width), str(height)], env={**self.env, **extra_env}, stdout=path.open("wb"), stderr=subprocess.STDOUT)
        self.clients.append((process, path))
        wait(lambda: "keyboard-enter" in path.read_text(), "first-map focus")
        return path

    def command(self, kind, payload=b""):
        self.process.stdin.write(struct.pack("<BBHI", 8, kind, 0, len(payload)) + payload)
        self.process.stdin.flush()

    def input(self, kind, fmt, *values):
        self.sequence += 1
        self.command(kind, struct.pack("<" + fmt + "I", *values, self.sequence))

    def motion(self, x, y):
        self.input(1, "II", round(x / self.size[0] * 65535), round(y / self.size[1] * 65535))

    def button(self, pressed, code=272):
        self.input(2, "IB", code, int(pressed))

    def key(self, code):
        self.input(4, "IB", code, 1)
        self.input(4, "IB", code, 0)

    def drain(self):
        while select.select([self.process.stdout], [], [], 0)[0]:
            data = os.read(self.process.stdout.fileno(), 65536)
            assert data, self.log.read_text()
            self.pending += data
        while len(self.pending) >= 8:
            version, kind, reserved, length = struct.unpack("<BBHI", self.pending[:8])
            assert version == 8 and reserved == 0, "stdout protocol corrupted"
            if len(self.pending) < 8 + length:
                break
            self.events.append((kind, self.pending[8:8+length]))
            self.pending = self.pending[8+length:]
        return self.events

    def frame(self, generation=1):
        path = self.directory / f"{generation}-{self.size[0]}x{self.size[1]}.bgra"
        self.drain()
        return path.read_bytes() if path.exists() else b""

    def pixel(self, frame, x, y):
        offset = (y * self.size[0] + x) * 4
        return tuple(frame[offset:offset+3])


def main():
    with tempfile.TemporaryDirectory(prefix="waywire-native-test-") as temporary:
        scene = Scene(Path(temporary))
        try:
            first = scene.client(300, 210)
            assert "bounds 800 600" in first.read_text()
            caps = re.search(r"wm-capabilities([ \d]+)", first.read_text())
            assert set(map(int, caps[1].split())) == {2, 3}, "advertise only maximize/fullscreen"
            wait(lambda: "text-enter" in first.read_text(), "text input focus")
            time.sleep(0.05)
            scene.sequence += 1
            scene.command(10, struct.pack("<BI", 1, scene.sequence) + "pré".encode())
            wait(lambda: "preedit pré 4 4" in first.read_text(), "UTF-8 preedit cursor byte positions")
            scene.sequence += 1
            scene.command(10, struct.pack("<BI", 0, scene.sequence) + "日本 73".encode())
            wait(lambda: "commit 日本 73" in first.read_text() and "preedit  0 0" in first.read_text(), "text commit clears preedit")
            # Surface origin is discovered from pointer events rather than assumed.
            scene.motion(100, 100)
            wait(lambda: "pointer-enter" in first.read_text(), "pointer enters first client")
            match = re.search(r"pointer-enter ([\d.]+) ([\d.]+)", first.read_text())
            x, y = (round(100 - float(v)) for v in match.groups())
            wait(lambda: scene.pixel(scene.frame(), x+20, y+20) == (32, 48, 255), "red top-left without channel swap")
            pixels = scene.frame()
            assert scene.pixel(pixels, x+150, y+20) == (96, 176, 32)
            assert scene.pixel(pixels, x+20, y+100) == (224, 80, 48)
            assert scene.pixel(pixels, x+150, y+100) == (48, 176, 224)
            assert scene.pixel(pixels, x+150, y-10) != (31, 23, 20), "decoration missing"

            second = scene.client(180, 120)
            scene.key(30)
            wait(lambda: "key 30 1" in second.read_text(), "new window keyboard focus")
            assert "key 30 1" not in first.read_text()
            if screenshot := os.environ.get("WAYWIRE_TEST_SCREENSHOT"):
                time.sleep(0.1)
                subprocess.run(["magick", "-size", "800x600", "-depth", "8", "bgra:-", screenshot], input=scene.frame(), check=True)
            # First window's exposed corner raises it above the second.
            scene.motion(x+10, y+10)
            scene.button(True); scene.button(False)
            scene.key(48)
            wait(lambda: "key 48 1" in first.read_text(), "click raises and focuses lower window")
            assert "key 48 1" not in second.read_text()
            wait(lambda: scene.pixel(scene.frame(), x+110, y+80) == (48, 176, 224), "raised first window occludes second")

            # Drag the server bar, then resize from a client-validated top-left grab.
            scene.motion(x+40, y-10); scene.button(True)
            scene.motion(x+87, y+19); scene.button(False)
            x += 47; y += 29
            wait(lambda: scene.pixel(scene.frame(), x+20, y+20) == (32, 48, 255), "SSD move updates rendered origin")
            scene.motion(x+30, y+30); scene.button(True, 273)
            time.sleep(0.1)
            scene.motion(x+53, y+47); scene.button(False, 273)
            wait(lambda: "paint 277 193" in first.read_text(), "top-left resize preserves opposite edge")
            x += 23; y += 17
            wait(lambda: scene.pixel(scene.frame(), x+10, y+10) == (32, 48, 255), "resize buffer anchored at committed geometry")
            scene.motion(x+40, y+40); scene.button(True, 274)
            wait(lambda: "popup-painted" in first.read_text(), "popup created from valid pointer grab")
            scene.button(False, 274)
            match = re.search(r"popup-configure (-?\d+) (-?\d+) (\d+) (\d+)", first.read_text())
            px, py, pw, ph = map(int, match.groups())
            assert 0 <= x+px <= 800-pw and 0 <= y+py <= 600-ph, "popup constrained to output"
            wait(lambda: scene.pixel(scene.frame(), x+px+10, y+py+10) == (32, 48, 255), "popup rendered outside parent bounds")
            scene.motion(5, 580); scene.button(True); scene.button(False)
            wait(lambda: "popup-done" in first.read_text(), "outside click dismisses grabbed popup")
            scene.motion(x+40, y+40); scene.button(True); scene.button(False)
            scene.command(11, struct.pack("<IB", 1, 1))
            scene.key(34)
            wait(lambda: "frame-done 3" in first.read_text(), "frame callbacks continue through four commits")
            wait(lambda: scene.pixel(scene.frame(), x+10, y+10) == (36, 48, 255), "damage after cached keyframe renders latest shm commit")
            scene.input(4, "IB", 42, 1)
            scene.input(4, "IB", 42, 2)
            scene.command(5)
            wait(lambda: "key 42 0" in first.read_text(), "ReleaseAll clears held key")
            assert first.read_text().count("key 42 1") == 1, "browser repeat doubled client repeat"

            # Invalid serial cannot initiate a resize or move the window.
            scene.key(23)
            scene.motion(x+90, y+70)
            time.sleep(0.1)
            assert "paint 277 193" in first.read_text()
            assert scene.pixel(scene.frame(), x+10, y+10) == (36, 48, 255)

            scene.key(46)
            wait(lambda: (1, b"native clipboard asymmetric 73") in scene.drain(), "native clipboard reaches browser pipe")
            scene.command(7, "browser clipboard β 91".encode())
            time.sleep(0.1); scene.key(47)
            wait(lambda: "clipboard browser clipboard β 91" in first.read_text(), "browser clipboard reaches native fd")
            prior = len(scene.drain())
            scene.key(45)
            wait(lambda: (1, b"") in scene.drain()[prior:], "native clipboard clear reaches browser")

            for code, size in [(50, (800, 570)), (33, (800, 600)), (19, (800, 570)), (22, (277, 193)),
                               (33, (800, 600)), (50, (800, 600)), (22, (800, 600)), (19, (277, 193))]:
                offset = len(first.read_text())
                scene.key(code)
                wait(lambda: f"paint {size[0]} {size[1]}" in first.read_text()[offset:], "independent maximize/fullscreen restoration")
            scene.key(50)
            wait(lambda: "paint 800 570" in first.read_text(), "maximize leaves decoration room")
            scene.motion(799, 599)
            scene.command(6, struct.pack("<IIHH", 630, 450, 180, 1))
            scene.size = (630, 450)
            wait(lambda: "paint 420 270" in first.read_text() and "scale 180" in first.read_text(), "live fractional resize reconfigures maximized client")
            assert "bounds 420 270" in first.read_text()
            wait(lambda: any(k == 3 and struct.unpack("<HIIHI", p) == (1, 630, 450, 180, 2) for k, p in scene.drain()), "resize generation acknowledgment")
            wait(lambda: any(k == 2 and struct.unpack_from("<I", p)[0] == 2 for k, p in scene.drain()), "encoder-submitted new generation frame")
            cursor = [struct.unpack("<II", p) for k, p in scene.drain() if k == 6][-1]
            assert cursor == (round(629/630*65535), round(449/450*65535)), cursor
            wait(lambda: len(scene.frame(2)) == 630*450*4, "fractional render dimensions")
            # Close the decorated maximized client; next live client regains focus.
            scene.motion(615, 12); scene.button(True); scene.button(False)
            wait(lambda: "close" in first.read_text(), "SSD close is graceful")
            wait(lambda: second.read_text().count("keyboard-enter") == 2, "focus restored after client disconnect")
            scene.key(32)
            wait(lambda: "key 32 1" in second.read_text(), "focus restored after close")
            # A 400x260 client fits the 420x300 logical output only when its
            # cascade is clamped to (20,34), reserving the 24px bar and 6px strip.
            large = scene.client(400, 260)
            scene.motion(60, 81)  # Physical (60,81) is logical (40,54) at 1.5x.
            wait(lambda: "pointer-enter" in large.read_text(), "large initial window stays on output")
            match = re.search(r"pointer-enter ([\d.]+) ([\d.]+)", large.read_text())
            assert all(abs(float(value) - 20) < 0.02 for value in match.groups()), match.groups()
            # A client-side decoration inset moves the surface origin, not the
            # window geometry. Rendering and input must agree on that offset.
            inset = scene.client(120, 90, WAYWIRE_TEST_GEOMETRY="1")
            scene.motion(240, 225)
            wait(lambda: "pointer-enter" in inset.read_text(), "inset window receives input")
            match = re.search(r"pointer-enter ([\d.]+) ([\d.]+)", inset.read_text())
            assert all(abs(float(value) - expected) < 0.02 for value, expected in zip(match.groups(), (49, 51))), match.groups()
            wait(lambda: scene.pixel(scene.frame(2), 240, 225) == (48, 176, 224), "CSD pixels align with surface-local input")
            scene.key(38)  # A later client commit grows the buffer to 360x250.
            wait(lambda: "paint 360 250" in inset.read_text(), "client-driven growth")
            scene.motion(180, 126)
            # Geometry is now (77,73,343,221), surface origin (60,44).
            wait(lambda: any(abs(float(x)-60) < 0.02 and abs(float(y)-40) < 0.02
                for x, y in re.findall(r"(?:motion|pointer-enter) ([\d.]+) ([\d.]+)", inset.read_text())),
                "later client size change stays on output")
            wait(lambda: scene.pixel(scene.frame(2), 180, 126) == (32, 48, 255), "grown client pixels align with input")
            assert all(a < b for a, b in zip(
                [struct.unpack_from("<Q", p, 16)[0] for k, p in scene.events if k == 2],
                [struct.unpack_from("<Q", p, 16)[0] for k, p in scene.events if k == 2][1:])), "frame sequence is monotonic"
            print("PASS: asymmetric BGRA, text-input-v3, stacking/focus, SSD move/close, top-left resize, popup constrain/grab/dismiss, frame callbacks/damage, repeat/release, bidirectional clipboard, nested maximize/fullscreen, fractional resize/generation/cursor, restored focus, initial placement, CSD input/render alignment")
        except Exception:
            print(scene.log.read_text(), file=sys.stderr)
            for _, log in scene.clients:
                print(log.name, log.read_text(), file=sys.stderr)
            raise
        finally:
            scene.close()


if __name__ == "__main__":
    if "-video_size" in sys.argv:
        encoder()
    else:
        main()
