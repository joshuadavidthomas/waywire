import assert from "node:assert/strict";
import test from "node:test";

import { WaywireSession, type QualityPreset } from "./waywire.ts";
import {
  FakeWebSocket,
  flush,
  installBrowser,
  socket,
  surfaceOptions,
} from "./test-support.ts";

function presets(control: FakeWebSocket): unknown[] {
  return control.sent.flatMap((value) => {
    if (typeof value !== "string") return [];
    const message = JSON.parse(value);
    return message.type === "set-quality" ? [message.preset] : [];
  });
}

function active(control: FakeWebSocket): void {
  control.dispatch("message", {
    data: JSON.stringify({ type: "control-state", state: "active" }),
  });
}

test("quality is restored on ownership, changed live, and retained through reconnect", async () => {
  installBrowser();
  const controls: FakeWebSocket[] = [];
  const session = new WaywireSession({
    endpoint: "https://remote.example.com",
    createWebSocket(path) {
      const created = new FakeWebSocket();
      if (path === "/control") controls.push(created);
      return socket(created);
    },
  });
  session.attachSurface(surfaceOptions());
  try {
    session.video.setQuality("medium");
    session.input.acquire();
    session.connect();
    await flush();
    const control = controls[0];
    assert.ok(control);
    control.readyState = FakeWebSocket.OPEN;
    control.dispatch("open", {});
    assert.deepEqual(
      presets(control),
      [],
      "nonowners must not change the encoder",
    );
    active(control);
    assert.deepEqual(presets(control), ["medium"]);
    session.video.setQuality("high");
    session.video.setQuality("automatic");
    assert.deepEqual(presets(control), ["medium", "high", "automatic"]);
    session.input.release();
    session.video.setQuality("low");
    assert.deepEqual(presets(control), ["medium", "high", "automatic"]);
    session.input.acquire();
    active(control);
    assert.deepEqual(presets(control), ["medium", "high", "automatic", "low"]);
    session.disconnect();
    session.connect();
    session.input.acquire();
    await flush();
    const reconnected = controls[1];
    assert.ok(reconnected);
    reconnected.readyState = FakeWebSocket.OPEN;
    reconnected.dispatch("open", {});
    active(reconnected);
    assert.deepEqual(presets(reconnected), ["low"]);
    assert.throws(
      () => session.video.setQuality("ultra" as QualityPreset),
      /Unknown quality preset/,
    );
    assert.deepEqual(presets(reconnected), ["low"]);
  } finally {
    await session.dispose();
  }
  assert.throws(() => session.video.setQuality("automatic"), /disposed/);
});
