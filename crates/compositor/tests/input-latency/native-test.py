#!/usr/bin/env python3
"""Check real native pixel commits, including delayed/superseded responses."""
import importlib.util
from pathlib import Path
import tempfile
import time

spec = importlib.util.spec_from_file_location("scene", Path(__file__).resolve().parents[1] / "native-scene.py")
scene_module = importlib.util.module_from_spec(spec)
spec.loader.exec_module(scene_module)


def marker(scene):
    frame = scene.frame()
    if not frame:
        return None
    if scene.pixel(frame, 48, 80) != (255, 0, 255):
        return None
    if scene.pixel(frame, 48, 112) != (255, 255, 0):
        return None
    result = 0
    for bit in range(16):
        a = scene.pixel(frame, 80 + 32 * bit, 80)
        b = scene.pixel(frame, 80 + 32 * bit, 112)
        if a == (238, 238, 238) and b == (17, 17, 17):
            result |= 1 << bit
        elif a != (17, 17, 17) or b != (238, 238, 238):
            return None
    return result


for mode in ["quiet", "motion"]:
    with tempfile.TemporaryDirectory(prefix="waywire-latency-test-") as directory:
        scene = scene_module.Scene(Path(directory))
        try:
            log = scene.client(800, 600, WAYWIRE_LATENCY=mode, WAYWIRE_TEST_ACTIONS="", WAYWIRE_TEST_DECORATIONS="")
            scene_module.wait(lambda: marker(scene) == 0, "initial pixel marker")
            background = scene.frame()[128 * 800 * 4:]
            scene.key(67)
            scene_module.wait(lambda: "key 67 1" in log.read_text(), "native delayed key arrival")
            time.sleep(.12)
            assert marker(scene) == 0, "processing the key must not imply responding pixels"
            scene_module.wait(lambda: marker(scene) == 1, "delayed marker 1")
            scene.key(67)
            time.sleep(.05)
            scene.key(66)
            scene_module.wait(lambda: marker(scene) == 3, "new response supersedes delayed response")
            time.sleep(.4)
            assert marker(scene) == 3, "stale delayed response overwrote newer response"
            for expected in range(4, 25):
                scene.key(66)
                scene_module.wait(lambda: marker(scene) == expected, f"exact native marker {expected}")
                if mode == "quiet":
                    assert scene.frame()[128 * 800 * 4:] == background, "quiet buffer rotation changed the background"
            print(f"PASS {mode}: delayed pixel response and stale-response supersession")
        finally:
            scene.close()
