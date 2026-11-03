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
import { TrackSink } from "./track-sink.ts";
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
  const canvas = new FakeTarget();
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
      requestKeyFrame() {},
      createTrackSink: () =>
        TrackSink.over(canvas as unknown as HTMLCanvasElement),
      displayFullscreen: () =>
        (document as unknown as { fullscreenElement?: unknown })
          .fullscreenElement === canvas,
    },
    latency,
    0,
  );
  video.attach(
    canvas as unknown as HTMLCanvasElement,
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
    canvas,
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
    async feedRaw(data: ArrayBuffer) {
      stream.dispatch("message", { data });
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

test("a frame answering this viewer's input skips the playout delay", async (context) => {
  const h = await presentation(context, 300);
  try {
    await h.feed(0, { keyframe: true });
    h.advance(10);
    await h.feed(10_000);
    h.paint();
    assert.deepEqual(h.draws, [], "a frame without new input waits");
    h.advance(10);
    await h.feed(20_000, { inputSequence: 1 });
    assert.deepEqual(
      h.draws,
      [{ timestamp: 20_000, at: 20 }],
      "drawn on decode, not at the next animation frame",
    );
    h.advance(10);
    await h.feed(30_000, { inputSequence: 1 });
    h.advance(10);
    h.paint();
    assert.equal(h.draws.length, 1, "the same input does not answer twice");
    h.advance(300);
    h.paint();
    assert.deepEqual(h.draws.at(-1), { timestamp: 30_000, at: 340 });
  } finally {
    h.video.disconnect();
  }
  assert.equal(h.closed.length, 4);
});

test("a striped frame answering this viewer's input skips the playout delay", async (context) => {
  installGlobal(
    "OffscreenCanvas",
    class {
      constructor(
        readonly width: number,
        readonly height: number,
      ) {}
      getContext() {
        return { drawImage() {} };
      }
    },
  );
  installGlobal(
    "VideoFrame",
    class {
      readonly displayWidth = 1280;
      readonly displayHeight = 720;
      readonly timestamp: number;
      constructor(_source: unknown, init: { timestamp: number }) {
        this.timestamp = init.timestamp;
      }
      close() {}
    },
  );
  // One full-height stripe: y u16, height u16, key flag u8, length u32, then
  // an access unit whose sequence parameter set names the codec.
  const stripe = (key: boolean) =>
    new Uint8Array([
      0,
      0,
      208,
      2,
      key ? 1 : 0,
      9,
      0,
      0,
      0,
      0,
      0,
      0,
      1,
      0x67,
      0x42,
      0xc0,
      0x1f,
      0xda,
    ]);
  const striped = (timestamp: number, key: boolean, inputSequence = 0) => {
    const data = new Uint8Array(
      videoPacket(timestamp, {
        keyframe: key,
        inputSequence,
        data: stripe(key),
      }),
    );
    data[1] = 2;
    return data.buffer;
  };
  const h = await presentation(context, 300);
  try {
    await h.feedRaw(striped(0, true));
    h.advance(10);
    await h.feedRaw(striped(10_000, false));
    h.paint();
    assert.deepEqual(h.draws, [], "a frame without new input waits");
    h.advance(10);
    await h.feedRaw(striped(20_000, false, 1));
    assert.deepEqual(h.draws, [{ timestamp: 20_000, at: 20 }]);
  } finally {
    h.video.disconnect();
  }
});

test("where MediaStreamTrackGenerator exists, frames show through a <video> over the canvas", async (context) => {
  const written: number[] = [];
  let stopped = false;
  installGlobal(
    "MediaStreamTrackGenerator",
    class {
      readonly writable = {
        getWriter: () => ({
          write(frame: VideoFrame) {
            written.push(frame.timestamp);
            frame.close();
            return Promise.resolve();
          },
          close: () => Promise.resolve(),
        }),
      };
      stop() {
        stopped = true;
      }
    },
  );
  installGlobal(
    "MediaStream",
    class {
      constructor(readonly tracks: unknown[]) {}
    },
  );
  installGlobal(
    "ResizeObserver",
    class {
      observe() {}
      disconnect() {}
    },
  );
  installGlobal("getComputedStyle", () => ({
    objectFit: "contain",
    objectPosition: "center",
  }));
  const h = await presentation(context, 300);
  (document as unknown as { createElement: () => FakeTarget }).createElement =
    () =>
      Object.assign(new FakeTarget(), {
        play: () => Promise.resolve(),
        readyState: 0,
      });
  const stage = new FakeTarget();
  stage.append(h.canvas);
  Object.assign(h.canvas, {
    after: (element: FakeTarget) => stage.append(element),
  });
  try {
    await h.feed(0, { keyframe: true });
    h.advance(300);
    h.paint();
    assert.deepEqual(
      h.draws.map(({ timestamp }) => timestamp),
      [0],
      "the canvas shows frames until the <video> has one",
    );
    const video = stage.children[1];
    assert.equal(stage.children.length, 2);
    Object.assign(video!, { readyState: 2 });
    await h.feed(310_000, { inputSequence: 1 });
    assert.deepEqual(written, [0, 310_000]);
    assert.deepEqual(
      h.draws.map(({ timestamp }) => timestamp),
      [0],
      "the canvas is not drawn once the <video> shows frames",
    );
    assert.equal(
      (video?.style as unknown as { pointerEvents: string }).pointerEvents,
      "none",
      "input stays on the canvas",
    );
    await h.feed(315_000, { inputSequence: 1 });
    h.advance(315);
    assert.deepEqual(
      written,
      [0, 310_000, 315_000],
      "a due frame shows without waiting for an animation frame, which Chromium slows to the video's rate",
    );
    await h.feed(318_000, { inputSequence: 1 });
    assert.deepEqual(written, [0, 310_000, 315_000], "not due for 3 ms");
    h.advance(3);
    assert.deepEqual(
      written,
      [0, 310_000, 315_000, 318_000],
      "a frame due within the animation-frame lead waits until it is due",
    );
    Object.assign(document, { fullscreenElement: h.canvas });
    h.advance(10);
    await h.feed(320_000, { inputSequence: 2 });
    assert.deepEqual(
      h.draws.map(({ timestamp }) => timestamp),
      [0, 320_000],
      "a fullscreen canvas is drawn too",
    );
    assert.deepEqual(written, [0, 310_000, 315_000, 318_000, 320_000]);
    h.video.detach();
    assert.equal(stage.children.length, 1);
    assert.ok(stopped);
  } finally {
    h.video.disconnect();
    delete (globalThis as { MediaStreamTrackGenerator?: unknown })
      .MediaStreamTrackGenerator;
  }
});

test("feedback reports the video bytes that arrived in each interval", async (context) => {
  const h = await presentation(context, 0);
  try {
    const sizes = [5_000, 700, 1_300];
    let expected = 0;
    for (const [i, size] of sizes.entries()) {
      const packet = videoPacket(i * 1000, {
        keyframe: i === 0,
        sequence: i,
        data: new Uint8Array(size),
      });
      expected += packet.byteLength;
      await h.feedRaw(packet);
    }
    // Every packet counts, including its header, whether or not it was drawn.
    assert.equal(h.draws.length, 0);
    assert.ok(expected > 7_000);
    assert.equal(h.video.takeFeedback(1000)?.bytes, expected);
    assert.equal(h.video.takeFeedback(2000)?.bytes, 0);
  } finally {
    h.video.disconnect();
  }
});

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

test("frames the gateway dropped on a slow link count as loss in feedback", async (context) => {
  const h = await presentation(context, 0);
  const feed = async (
    timestamp: number,
    options: Parameters<typeof h.feed>[1],
  ) => {
    await h.feed(timestamp, options);
    // Present each frame before the next, so only link loss is counted.
    h.advance(1_000);
    h.paint();
  };
  try {
    await feed(1_000, { keyframe: true, sequence: 10 });
    await feed(2_000, { sequence: 11 });
    // The gateway's queue overflowed: it dropped 12..18 and resumed at a key frame.
    await feed(9_000, { keyframe: true, discontinuity: true, sequence: 19 });
    // Without a discontinuity a gap is a frame the compositor never sent.
    await feed(10_000, { sequence: 23 });
    // A new generation restarts the sequence space; its gap is not loss.
    await feed(11_000, {
      keyframe: true,
      discontinuity: true,
      generation: 2,
      sequence: 40,
    });
    assert.equal(h.video.stats.linkDroppedFrames, 7);
    assert.equal(h.video.stats.decoderResetDroppedFrames, 0);
    assert.equal(h.video.takeFeedback()?.dropped, 7);
  } finally {
    h.video.disconnect();
  }
});
