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
  assert.deepEqual(fitObservedResize(6000, 6000, { maxPixels: 36_000_000 }), {
    width: 6000,
    height: 6000,
    downscale: 1,
  });
  assert.deepEqual(normalizeResizeDimensions(6002, 6002), {
    width: 6000,
    height: 6000,
  });
});
