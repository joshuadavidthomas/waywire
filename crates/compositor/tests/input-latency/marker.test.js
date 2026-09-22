import assert from "node:assert/strict";
import { test } from "node:test";
import { decodeMarker } from "./marker.js";

function rows(value) {
  return [0, 1].map((row) => {
    const data = new Uint8ClampedArray(544 * 4);
    for (let cell = 0; cell < 17; cell++) {
      const color =
        cell === 0
          ? row
            ? [0, 255, 255]
            : [255, 0, 255]
          : Array(3).fill(((value >>> (cell - 1)) & 1) ^ row ? 238 : 17);
      for (let x = cell * 32; x < cell * 32 + 32; x++)
        data.set([...color, 255], x * 4);
    }
    return data;
  });
}
test("asymmetric marker values and all 16 bits are independently decoded", () => {
  for (const marker of [
    0, 1, 3, 15, 16, 17, 18, 19, 20, 24, 255, 32768, 42301, 65535,
  ])
    assert.equal(decodeMarker(rows(marker), 1), marker);
});
test("stale, partial and missing-anchor marker cannot masquerade as expected response", () => {
  assert.notEqual(decodeMarker(rows(412), 1), 413);
  const partial = [rows(413)[0], rows(412)[1]];
  assert.equal(decodeMarker(partial, 1), null);
  const corrupt = rows(413);
  corrupt[0].fill(0, 0, 32 * 4);
  assert.equal(decodeMarker(corrupt, 1), null);
  assert.equal(
    decodeMarker(
      rows(65535).map((row) => row.slice(0, 32 * 4)),
      1,
    ),
    null,
  );
});
