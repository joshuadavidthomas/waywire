import assert from "node:assert/strict";
import test, { type TestContext } from "node:test";
import { PROTOCOL_VERSION } from "./messages.ts";
import type { WaywireStats } from "./session.ts";
import {
  FakeTarget,
  FakeWebSocket,
  flush,
  installBrowser,
  installGlobal,
  installQueueVideoDecoder,
  socket,
  videoPacket,
} from "./test-support.ts";
import { VideoRuntime } from "./video.ts";

async function presentation(context: TestContext, latency: number) {
  installBrowser();
  const installed = installQueueVideoDecoder();
  let now = 0;
  context.mock.method(performance, "now", () => now);
  context.mock.timers.enable({ apis: ["setTimeout"] });
  const callbacks = new Map<number, FrameRequestCallback>();
  let callbackId = 0;
  installGlobal("requestAnimationFrame", (callback: FrameRequestCallback) => {
    callbacks.set(++callbackId, callback);
    return callbackId;
  });
  installGlobal("cancelAnimationFrame", (id: number) => callbacks.delete(id));
  const stream = new FakeWebSocket();
  const draws: Array<{ timestamp: number; at: number }> = [];
  const published: WaywireStats[] = [];
  const video = new VideoRuntime(
    {
      createSocket: async () => socket(stream),
      connectionGeneration: () => 1,
      shouldRun: () => true,
      disposed: () => false,
      setStatus() {},
      updateState() {},
      emitError(error) {
        throw error;
      },
      halt(error) {
        throw error;
      },
      expectedPresentationTime: (timestamp, target) =>
        timestamp / 1000 + target,
      statsContext: () => ({
        bitrateKbps: 16000,
        scalePercent: 100,
        rttMs: 0,
        clockConfident: true,
        clockUncertaintyMs: 0,
        pendingInputCount: 0,
        resizeState: "idle",
      }),
      setLatestAppliedInput() {},
      presentResizeGeneration() {},
      publishStats: (stats) => published.push(stats),
    },
    latency,
    0,
  );
  video.attach(
    new FakeTarget() as unknown as HTMLCanvasElement,
    {
      drawImage(frame: VideoFrame) {
        draws.push({ timestamp: frame.timestamp, at: now });
      },
    } as unknown as CanvasRenderingContext2D,
  );
  await video.connect();
  stream.dispatch("message", {
    data: JSON.stringify({
      type: "video-config",
      version: PROTOCOL_VERSION,
      codec: "avc1.F40034",
    }),
  });
  await flush();
  video.resetFeedbackInterval(0);
  return {
    video,
    draws,
    published,
    closed: installed.closedTimestamps,
    async feed(
      timestamp: number,
      options: Parameters<typeof videoPacket>[1] = {},
    ) {
      stream.dispatch("message", { data: videoPacket(timestamp, options) });
      await flush();
      installed.decoder()?.outputAll();
    },
    advance(milliseconds: number) {
      now += milliseconds;
      context.mock.timers.tick(milliseconds);
    },
    paint() {
      const ready = [...callbacks.values()];
      callbacks.clear();
      for (const callback of ready) callback(now);
    },
  };
}

for (const [fps, latency] of [
  [60, 300],
  [90, 300],
  [120, 300],
  [120, 1000],
] as const) {
  test(`${fps} FPS at ${latency}ms buffers until due and presents at 60Hz without overflow`, async (context) => {
    const h = await presentation(context, latency);
    const captures: number[] = [];
    let nextFrame = 0;
    let nextPaint = 0;
    try {
      for (let time = 0; time <= latency + 1100; time++) {
        if (time) h.advance(1);
        if (time >= nextFrame) {
          captures.push(time * 1000);
          await h.feed(time * 1000, { keyframe: time === 0, fps });
          nextFrame += 1000 / fps;
        }
        if (time >= nextPaint) {
          h.paint();
          nextPaint += 1000 / 60;
        }
      }
      // Allow phase/rounding at coincident arrival and rAF deadlines, but require
      // at least a full second of presentation within the 1.1-second due window.
      assert.ok(h.draws.length >= 60, `only ${h.draws.length} draws`);
      for (const draw of h.draws) {
        const newestDue = captures
          .filter((timestamp) => timestamp / 1000 + latency <= draw.at)
          .at(-1);
        assert.equal(
          draw.timestamp,
          newestDue,
          "must draw newest due, never future frames",
        );
      }
      assert.equal(h.video.stats.decodedOverflowDroppedFrames, 0);
      assert.ok(
        (h.video.stats.pendingVideoFrames ?? Infinity) <=
          Math.ceil((fps * latency) / 1000) + 2,
      );
      const feedback = h.video.takeFeedback();
      assert.equal(
        feedback?.dropped,
        0,
        "presentation skips are not capacity loss",
      );
      if (fps > 60) assert.ok((h.video.stats.overdueDroppedFrames ?? 0) > 0);
      h.video.setLatencyTarget(0);
      h.paint();
      assert.equal(h.draws.at(-1)?.timestamp, captures.at(-1));
    } finally {
      h.video.disconnect();
    }
    assert.equal(
      h.closed.length,
      captures.length,
      "every decoded frame must close exactly once",
    );
    assert.equal(new Set(h.closed).size, h.closed.length);
  });
}

test("stalled presentation exposes live loss without publishing imaginary draws", async (context) => {
  const h = await presentation(context, 0);
  try {
    for (let i = 0; i < 50; i++) {
      await h.feed(i * 1000, { keyframe: i === 0, fps: 120 });
      h.advance(1);
    }
    assert.equal(h.draws.length, 0);
    assert.equal(h.published.length, 0);
    assert.equal(h.video.stats.drawCompletedAtMs, undefined);
    assert.equal(h.video.stats.receivedFrames, 50);
    assert.equal(h.video.stats.decodedFrames, 50);
    assert.equal(h.video.stats.pendingVideoFrames, 24);
    assert.equal(h.video.stats.decodedOverflowDroppedFrames, 26);
    assert.equal(h.video.takeFeedback()?.dropped, 26);
    h.paint();
    assert.equal(h.draws.at(-1)?.timestamp, 49_000);
    assert.equal(h.video.takeFeedback(51)?.dropped, 0);
    const lastDraw = h.video.stats.drawCompletedAtMs;
    h.advance(2000);
    assert.equal(h.video.stats.renderedFps, 0);
    assert.equal(h.video.stats.drawCompletedAtMs, lastDraw);
    assert.equal(h.published.length, 1);
  } finally {
    h.video.disconnect();
  }
  assert.equal(h.closed.length, 50);
});
