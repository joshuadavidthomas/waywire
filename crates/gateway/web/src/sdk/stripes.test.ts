import assert from "node:assert/strict";
import test from "node:test";

import { h264Codec, parseStripes } from "./stripes.ts";

test("stripe container parses each stripe and rejects malformed input", () => {
  // Matches push_stripe's layout in crates/protocol/src/browser.rs.
  const data = new Uint8Array([
    90, 0, 180, 0, 1, 2, 0, 0, 0, 0xaa, 0xbb, 0, 1, 64, 0, 0, 3, 0, 0, 0, 1, 2,
    3,
  ]);
  const stripes = parseStripes(data);
  assert.deepEqual(
    stripes?.map(({ y, height, key, data }) => [y, height, key, [...data]]),
    [
      [90, 180, true, [0xaa, 0xbb]],
      [256, 64, false, [1, 2, 3]],
    ],
  );
  assert.deepEqual(parseStripes(new Uint8Array()), []);
  assert.equal(parseStripes(data.subarray(0, data.byteLength - 1)), null);
  const badFlags = data.slice();
  badFlags[4] = 2;
  assert.equal(parseStripes(badFlags), null);
});

test("codec string comes from the sequence parameter set", () => {
  const unit = new Uint8Array([
    0, 0, 0, 1, 0x09, 0xf0, 0, 0, 0, 1, 0x67, 0x42, 0xc0, 0x1f, 0xda,
  ]);
  assert.equal(h264Codec(unit), "avc1.42C01F");
  assert.equal(h264Codec(new Uint8Array([0, 0, 0, 1, 0x65, 1, 2, 3, 4])), null);
});
