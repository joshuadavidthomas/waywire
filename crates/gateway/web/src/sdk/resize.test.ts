import assert from "node:assert/strict";
import test from "node:test";

import { fitObservedResize, normalizeResizeDimensions } from "./resize.ts";
import "./test-support.ts";

test("observed resize preserves fitting math and the protocol envelope", () => {
  assert.deepEqual(fitObservedResize(1920, 1080), {
    width: 1920,
    height: 1080,
    downscale: 1,
  });
  const tall = fitObservedResize(1922.5, 2467.5, {
    maxPixels: 2560 * 1440,
  });
  assert.equal(tall.width % 2, 0);
  assert.equal(tall.height % 2, 0);
  assert.ok(tall.width * tall.height <= 2560 * 1440);
  assert.ok(tall.height > 1440);
  // Policies above the compositor's pixel cap are clamped to it.
  assert.deepEqual(fitObservedResize(6000, 6000, { maxPixels: 36_000_000 }), {
    width: 2880,
    height: 2880,
    downscale: 0.48,
  });
  assert.deepEqual(normalizeResizeDimensions(6002, 6002), {
    width: 2880,
    height: 2880,
  });
  assert.deepEqual(
    (
      [
        [1200, 700],
        [1840, 900],
        [1180, 690],
        [1840, 900],
      ] as const
    ).map(([width, height]) => fitObservedResize(width, height)),
    [
      { width: 1200, height: 700, downscale: 1 },
      { width: 1840, height: 900, downscale: 1 },
      { width: 1180, height: 690, downscale: 1 },
      { width: 1840, height: 900, downscale: 1 },
    ],
  );
  assert.deepEqual(fitObservedResize(1146, 516), {
    width: 1146,
    height: 516,
    downscale: 1,
  });
});

test("normalized resizes fit the compositor's 3840x2160 pixel cap", () => {
  const maximumProtocolPixels = 3840 * 2160;
  // An asymmetric oversize request scales both sides down proportionally.
  assert.deepEqual(normalizeResizeDimensions(6000, 2000), {
    width: 4988,
    height: 1662,
  });
  // A square oversize request keeps its aspect ratio.
  assert.deepEqual(normalizeResizeDimensions(5000, 5000), {
    width: 2880,
    height: 2880,
  });
  for (const [width, height] of [
    [6000, 2000],
    [5000, 5000],
    [6000, 6000],
    [4000, 3000],
  ] as const) {
    const normalized = normalizeResizeDimensions(width, height);
    assert.ok(
      normalized.width * normalized.height <= maximumProtocolPixels,
      `${width}x${height} normalized to ${normalized.width}x${normalized.height}`,
    );
    assert.equal(normalized.width % 2, 0);
    assert.equal(normalized.height % 2, 0);
  }
});
