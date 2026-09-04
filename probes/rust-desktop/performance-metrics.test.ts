import assert from "node:assert/strict";
import test from "node:test";
import { longestFrameGap, hasExpectedQuality } from "./performance-metrics.js";

test("fixed-quality comparison rejects prior adaptation and any changed sample", () => {
  const expected = { bitrateKbps: 8000, scalePercent: 100 };
  assert(hasExpectedQuality([expected, expected]));
  assert(!hasExpectedQuality([]));
  assert(!hasExpectedQuality([{ ...expected, bitrateKbps: 6400 }]));
  assert(!hasExpectedQuality([expected, { ...expected, bitrateKbps: 6400 }]));
  assert(!hasExpectedQuality([{ ...expected, scalePercent: 80 }]));
});

test("freeze measurement includes internal gaps and both recording edges", () => {
  assert.equal(longestFrameGap(1000, [100, 400, 500, 700, 900]), 300);
  assert.equal(longestFrameGap(1000, [400, 500, 700, 900]), 400);
  assert.equal(longestFrameGap(1000, [100, 200, 300, 400]), 600);
});

test("a recording with no updates freezes for its entire duration", () => {
  assert.equal(longestFrameGap(30_000, []), 30_000);
  assert.equal(longestFrameGap(30_000, [0]), 30_000);
  assert.equal(longestFrameGap(30_000, [30_000]), 30_000);
});

test("invalid timing evidence fails rather than reducing the freeze", () => {
  for (const duration of [0, -1, NaN, Infinity])
    assert.throws(() => longestFrameGap(duration, []), RangeError);
  for (const updates of [[-1], [1001], [NaN], [Infinity], [500, 400]])
    assert.throws(() => longestFrameGap(1000, updates), RangeError);
});
