import assert from "node:assert/strict";
import test from "node:test";

import { PROTOCOL_VERSION } from "./messages.ts";
import { WaywireSession, type ResizeEvent } from "./session.ts";
import {
  FakeTarget,
  FakeWebSocket,
  flush,
  installBrowser,
  installGlobal,
  installQueueVideoDecoder,
  socket,
  surfaceOptions,
  videoPacket,
} from "./test-support.ts";

for (const discardedAt of ["decode", "render"] as const) {
  test(`resize presentation waits for drawing after a frame is discarded at ${discardedAt}`, async (t) => {
    installBrowser();
    const installed = installQueueVideoDecoder();
    let now = 100;
    t.mock.method(performance, "now", () => now);
    const animationFrames = new Map<number, FrameRequestCallback>();
    let nextAnimationFrame = 0;
    installGlobal("requestAnimationFrame", (callback: FrameRequestCallback) => {
      const id = ++nextAnimationFrame;
      animationFrames.set(id, callback);
      return id;
    });
    installGlobal("cancelAnimationFrame", (id: number) =>
      animationFrames.delete(id),
    );
    const draw = () => {
      const callbacks = [...animationFrames.values()];
      animationFrames.clear();
      for (const callback of callbacks) callback(now);
    };
    const sockets = new Map<string, FakeWebSocket>();
    const draws: number[] = [];
    const events: ResizeEvent[] = [];
    const canvas = new FakeTarget();
    canvas.getContext = () => ({
      drawImage(frame: VideoFrame) {
        draws.push(frame.timestamp);
      },
    });
    const session = new WaywireSession({
      endpoint: "https://remote.example.com",
      remoteDisplay: { mode: "fixed", width: 1600, height: 900, scale: 1 },
      createWebSocket(path) {
        const created = new FakeWebSocket();
        sockets.set(path, created);
        return socket(created);
      },
    });
    session.attachSurface(surfaceOptions(canvas));
    session.on("resize", (event) => {
      if (event.state === "presented") assert.ok(draws.length > 0);
      events.push(event);
    });
    try {
      session.input.acquire();
      session.connect();
      await flush();
      const control = sockets.get("/control");
      const video = sockets.get("/stream");
      assert.ok(control && video);
      control.readyState = FakeWebSocket.OPEN;
      control.dispatch("open", {});
      control.dispatch("message", {
        data: JSON.stringify({ type: "control-state", state: "active" }),
      });
      const resize = control.sent.find(
        (record): record is ArrayBuffer =>
          record instanceof ArrayBuffer &&
          new DataView(record).getUint8(1) === 6,
      );
      assert.ok(resize);
      now = 125;
      control.dispatch("message", {
        data: JSON.stringify({
          type: "resize-applied",
          request: new DataView(resize).getUint16(18, true),
          width: 1600,
          height: 900,
          scale: 120,
          generation: 2,
        }),
      });
      video.dispatch("message", {
        data: JSON.stringify({
          type: "video-config",
          version: PROTOCOL_VERSION,
          codec: "avc1.42E01E",
        }),
      });
      await new Promise<void>((resolve) => setImmediate(resolve));
      const decoder = installed.decoder();
      assert.ok(decoder);

      // A delta frame in a new generation cannot be decoded, much less presented.
      video.dispatch("message", {
        data: videoPacket(1_000, { generation: 2 }),
      });
      await flush();
      assert.equal(decoder.decodeQueueSize, 0);
      assert.deepEqual(
        events.map((event) => event.state),
        ["requested", "applied"],
      );

      video.dispatch("message", {
        data: videoPacket(2_000, {
          keyframe: true,
          generation: 2,
          width: 1200,
          height: 674,
        }),
      });
      await flush();
      if (discardedAt === "render") {
        decoder.outputAll();
        assert.equal(animationFrames.size, 1);
      }
      assert.deepEqual(
        events.map((event) => event.state),
        ["requested", "applied"],
      );

      // Reuse the timestamp across a generation boundary. Old metadata must be
      // discarded with the old frame, whether decoding or rendering was pending.
      video.dispatch("message", {
        data: videoPacket(2_000, {
          keyframe: true,
          generation: 3,
          width: 640,
          height: 360,
        }),
      });
      await flush();
      assert.equal(animationFrames.size, 0);
      now = 200;
      decoder.outputAll();
      await flush();
      assert.equal(animationFrames.size, 1);
      assert.deepEqual(draws, []);
      assert.deepEqual(
        events.map((event) => event.state),
        ["requested", "applied"],
      );
      now = 350;
      draw();
      assert.deepEqual(draws, [2_000]);
      assert.deepEqual(events.at(-1), {
        state: "presented",
        latencyMs: 250,
        width: 640,
        height: 360,
        generation: 3,
      });
      assert.equal(session.stats.generation, 3);
      assert.equal(session.stats.resizeState, "presented");
      assert.equal(session.stats.drawCompletedAtMs, 350);

      video.dispatch("message", {
        data: videoPacket(3_000, { generation: 3 }),
      });
      await flush();
      decoder.outputAll();
      draw();
      assert.equal(
        events.filter((event) => event.state === "presented").length,
        1,
      );
    } finally {
      await session.dispose();
    }
  });
}

for (const firstFrame of ["drawn", "superseded"] as const) {
  test(`a newer resize acknowledgement retains applied requests until ${firstFrame} frames settle them`, async () => {
    installBrowser();
    const installed = installQueueVideoDecoder();
    const callbacks = new Map<number, FrameRequestCallback>();
    let nextId = 0;
    installGlobal("requestAnimationFrame", (callback: FrameRequestCallback) => {
      callbacks.set(++nextId, callback);
      return nextId;
    });
    installGlobal("cancelAnimationFrame", (id: number) => callbacks.delete(id));
    const draw = () => {
      const pending = [...callbacks.values()];
      callbacks.clear();
      for (const callback of pending) callback(performance.now());
    };
    const sockets = new Map<string, FakeWebSocket>();
    const events: ResizeEvent[] = [];
    const canvas = new FakeTarget();
    canvas.getContext = () => ({ drawImage() {} });
    const session = new WaywireSession({
      endpoint: "https://remote.example.com",
      remoteDisplay: { mode: "fixed", width: 1600, height: 900, scale: 1 },
      createWebSocket(path) {
        const created = new FakeWebSocket();
        sockets.set(path, created);
        return socket(created);
      },
    });
    session.attachSurface(surfaceOptions(canvas));
    session.on("resize", (event) => events.push(event));
    try {
      session.input.acquire();
      session.connect();
      await flush();
      const control = sockets.get("/control");
      const video = sockets.get("/stream");
      assert.ok(control && video);
      const applied = (generation: number, requestIndex = -1) => {
        const record = control.sent
          .filter(
            (value): value is ArrayBuffer =>
              value instanceof ArrayBuffer &&
              new DataView(value).getUint8(1) === 6,
          )
          .at(requestIndex);
        assert.ok(record);
        const view = new DataView(record);
        control.dispatch("message", {
          data: JSON.stringify({
            type: "resize-applied",
            request: view.getUint16(18, true),
            width: view.getUint32(8, true),
            height: view.getUint32(12, true),
            scale: 120,
            generation,
          }),
        });
      };
      const frame = async (generation: number) => {
        video.dispatch("message", {
          data: videoPacket(generation * 1_000, { generation, keyframe: true }),
        });
        await flush();
        installed.decoder()?.outputAll();
      };
      control.readyState = FakeWebSocket.OPEN;
      control.dispatch("open", {});
      control.dispatch("message", {
        data: JSON.stringify({ type: "control-state", state: "active" }),
      });
      session.remoteDisplay.fixed({ width: 1280, height: 720, scale: 1 });
      applied(2, 0);
      video.dispatch("message", {
        data: JSON.stringify({
          type: "video-config",
          version: PROTOCOL_VERSION,
          codec: "avc1.42E01E",
        }),
      });
      await new Promise<void>((resolve) => setImmediate(resolve));
      await frame(1);
      draw();
      assert.equal(
        session.stats.resizeState,
        "requested",
        "A's acknowledgement must not hide unacknowledged B",
      );
      await frame(2);
      assert.equal(callbacks.size, 1);

      // B is acknowledged while A's decoded frame is still waiting for a draw.
      applied(3);
      assert.equal(
        events.filter((event) => event.state === "presented").length,
        0,
      );
      if (firstFrame === "drawn") {
        draw();
        assert.deepEqual(
          events
            .filter((event) => event.state === "presented")
            .map((event) => event.generation),
          [2],
        );
        assert.equal(
          session.stats.resizeState,
          "applied",
          "B is still awaiting presentation",
        );
      }
      await frame(3);
      draw();
      assert.deepEqual(
        events
          .filter((event) => event.state === "presented")
          .map((event) => event.generation),
        firstFrame === "drawn" ? [2, 3] : [3, 3],
      );
      assert.equal(session.stats.resizeState, "presented");

      // C never applies: the compositor coalesces it into D. C must not leave
      // the aggregate state waiting for an acknowledgement or frame forever.
      session.remoteDisplay.fixed({ width: 1024, height: 768, scale: 1 });
      session.remoteDisplay.fixed({ width: 1920, height: 1080, scale: 1 });
      applied(4);
      await frame(4);
      draw();
      assert.equal(
        events.filter((event) => event.state === "presented").length,
        3,
      );
      assert.equal(session.stats.resizeState, "presented");
    } finally {
      await session.dispose();
    }
  });
}
