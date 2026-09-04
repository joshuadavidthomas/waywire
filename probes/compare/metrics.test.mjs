import assert from "node:assert/strict";
import test from "node:test";
import { distribution, gaps, summarize } from "./metrics.mjs";

test("nearest-rank percentiles and empty samples", () => {
  assert.deepEqual(distribution([]), {
    count: 0,
    p50: null,
    p95: null,
    p99: null,
    max: null,
  });
  assert.deepEqual(distribution([4, 1, 3, 2]), {
    count: 4,
    p50: 2,
    p95: 4,
    p99: 4,
    max: 4,
  });
  assert.deepEqual(gaps([2, 32, 64, 200]), [30, 32, 136]);
});

test("byte rates use actual elapsed time and canvas gaps exclude idle boundaries", () => {
  const summary = summarize({
    durationMs: 2000,
    canvasSubmitsMs: [500, 505, 530, 730],
    canvasUpdateFramesMs: [516, 546, 746],
    animationFramesMs: [0, 16, 160],
    longTasks: [{ durationMs: 75 }],
    receivedBytes: 250000,
    sentBytes: 20,
    warnings: [],
  });
  assert.equal(summary.canvasSubmitsPerSecond, 2);
  assert.equal(summary.canvasUpdateFramesPerSecond, 1.5);
  assert.equal(summary.canvasUpdateFrameSpacingMs.p95, 200);
  assert.equal(summary.canvasSubmitSpacingMs.p95, 200);
  assert.equal(summary.canvasGapsOver100ms, 1);
  assert.equal(summary.browserAnimationGapsOver100ms, 1);
  assert.equal(summary.receivedPayloadMbps, 1);
  assert.equal(summary.longTaskTotalMs, 75);
});

test("missing updates are not presented as zero-latency success", () => {
  const summary = summarize({
    durationMs: 10,
    canvasSubmitsMs: [],
    canvasUpdateFramesMs: [],
    animationFramesMs: [],
    longTasks: [],
    receivedBytes: 0,
    sentBytes: 0,
    warnings: ["Tab hidden"],
  });
  assert.equal(summary.canvasSubmitSpacingMs.p95, null);
  assert.equal(summary.warnings.length, 2);
});
