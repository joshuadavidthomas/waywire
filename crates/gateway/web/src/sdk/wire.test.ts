import assert from "node:assert/strict";
import test from "node:test";

import { parseVideoPacket } from "./video.ts";
import {
  keyboardKey,
  pointerAbsolute,
  pointerButton,
  pointerRelative,
  pointerScroll,
  releaseAll,
  resize,
  videoFrame,
} from "./wire.ts";

function bytes(record: ArrayBuffer): number[] {
  return [...new Uint8Array(record)];
}

test("browser command encoders match the Rust v12 vectors", () => {
  assert.deepEqual(
    bytes(pointerAbsolute(12, 34, 7)),
    [12, 1, 0, 0, 12, 0, 0, 0, 12, 0, 0, 0, 34, 0, 0, 0, 7, 0, 0, 0],
  );
  assert.deepEqual(
    bytes(pointerButton(0x110, 1, 7)),
    [12, 2, 0, 0, 9, 0, 0, 0, 16, 1, 0, 0, 1, 7, 0, 0, 0],
  );
  assert.deepEqual(
    bytes(pointerScroll(1.5, -2.25, 7)),
    [12, 3, 0, 0, 12, 0, 0, 0, 0, 0, 192, 63, 0, 0, 16, 192, 7, 0, 0, 0],
  );
  assert.deepEqual(
    bytes(keyboardKey(30, 2, 7)),
    [12, 4, 0, 0, 9, 0, 0, 0, 30, 0, 0, 0, 2, 7, 0, 0, 0],
  );
  assert.deepEqual(bytes(releaseAll()), [12, 5, 0, 0, 0, 0, 0, 0]);
  assert.deepEqual(
    bytes(resize(1280, 720, 180, 9)),
    [12, 6, 0, 0, 12, 0, 0, 0, 0, 5, 0, 0, 208, 2, 0, 0, 180, 0, 9, 0],
  );
  assert.deepEqual(
    bytes(pointerRelative(1.5, -2.25, 7)),
    [12, 8, 0, 0, 12, 0, 0, 0, 0, 0, 192, 63, 0, 0, 16, 192, 7, 0, 0, 0],
  );
});

test("video parser decodes the Rust v12 frame vector", () => {
  const record = new Uint8Array([
    12, 1, 0, 0, 37, 0, 0, 0, 1, 1, 4, 0, 0, 0, 0, 5, 208, 2, 184, 130, 1, 0, 0,
    0, 0, 0, 17, 0, 0, 0, 0, 0, 0, 0, 8, 0, 0, 0, 60, 0, 0, 0, 1, 1, 2,
  ]).buffer;

  const packet = parseVideoPacket(record);

  assert.ok(packet);
  assert.equal(packet.keyframe, true);
  assert.equal(packet.discontinuity, true);
  assert.equal(packet.timestamp, 99);
  assert.equal(packet.generation, 4);
  assert.equal(packet.latestAppliedInput, 8);
  assert.equal(packet.width, 1280);
  assert.equal(packet.height, 720);
  assert.deepEqual([...packet.data], [1, 2]);
});

test("video parser rejects every truncated, extended or mislabeled frame", () => {
  // The Rust v12 frame vector above: an 8-byte header and a 37-byte payload.
  const frame = [
    12, 1, 0, 0, 37, 0, 0, 0, 1, 1, 4, 0, 0, 0, 0, 5, 208, 2, 184, 130, 1, 0, 0,
    0, 0, 0, 17, 0, 0, 0, 0, 0, 0, 0, 8, 0, 0, 0, 60, 0, 0, 0, 1, 1, 2,
  ];
  const parse = (bytes: number[]) =>
    parseVideoPacket(new Uint8Array(bytes).buffer);
  assert.ok(parse(frame));
  for (let length = 0; length < frame.length; length += 1)
    assert.equal(parse(frame.slice(0, length)), null, `${length}-byte prefix`);
  assert.equal(parse([...frame, 0]), null, "a byte past the declared length");
  // Each header byte other than the length: version, kind and the two
  // reserved bytes.
  for (const [index, value] of [
    [0, 13],
    [1, 3],
    [2, 1],
    [3, 1],
  ] as const) {
    const changed = [...frame];
    changed[index] = value;
    assert.equal(parse(changed), null, `header byte ${index} = ${value}`);
  }
  // Kind 2 is the same frame carrying the stripe container.
  const striped = [...frame];
  striped[1] = 2;
  assert.equal(parse(striped)?.striped, true);
  assert.equal(parse(frame)?.striped, false);
  // Frame kind and continuity are booleans on the wire.
  for (const index of [8, 9]) {
    const changed = [...frame];
    changed[index] = 2;
    assert.equal(parse(changed), null, `payload byte ${index} = 2`);
  }
});

test("video parser accepts every v12 format and rejects unknown chroma and v11", () => {
  const record = videoFrame(
    1,
    0,
    {
      generation: 1,
      width: 1280,
      height: 720,
      captureNanos: 1n,
      sequence: 1n,
      inputSequence: 0,
      fps: 60,
      chroma: 0,
    },
    new Uint8Array([1]),
  );
  for (const chroma of [0, 1, 2]) {
    new Uint8Array(record)[42] = chroma;
    assert.ok(parseVideoPacket(record));
  }
  new Uint8Array(record)[42] = 3;
  assert.equal(parseVideoPacket(record), null);
  new Uint8Array(record)[42] = 0;
  new Uint8Array(record)[0] = 11;
  assert.equal(parseVideoPacket(record), null);
});
