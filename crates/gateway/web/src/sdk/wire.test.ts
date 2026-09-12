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

test("browser command encoders match the Rust v7 vectors", () => {
  assert.deepEqual(
    bytes(pointerAbsolute(12, 34, 7)),
    [7, 1, 0, 0, 12, 0, 0, 0, 12, 0, 0, 0, 34, 0, 0, 0, 7, 0, 0, 0],
  );
  assert.deepEqual(
    bytes(pointerButton(0x110, 1, 7)),
    [7, 2, 0, 0, 9, 0, 0, 0, 16, 1, 0, 0, 1, 7, 0, 0, 0],
  );
  assert.deepEqual(
    bytes(pointerScroll(1.5, -2.25, 7)),
    [7, 3, 0, 0, 12, 0, 0, 0, 0, 0, 192, 63, 0, 0, 16, 192, 7, 0, 0, 0],
  );
  assert.deepEqual(
    bytes(keyboardKey(30, 2, 7)),
    [7, 4, 0, 0, 9, 0, 0, 0, 30, 0, 0, 0, 2, 7, 0, 0, 0],
  );
  assert.deepEqual(bytes(releaseAll()), [7, 5, 0, 0, 0, 0, 0, 0]);
  assert.deepEqual(
    bytes(resize(1280, 720, 180, 9)),
    [7, 6, 0, 0, 12, 0, 0, 0, 0, 5, 0, 0, 208, 2, 0, 0, 180, 0, 9, 0],
  );
  assert.deepEqual(
    bytes(pointerRelative(1.5, -2.25, 7)),
    [7, 8, 0, 0, 12, 0, 0, 0, 0, 0, 192, 63, 0, 0, 16, 192, 7, 0, 0, 0],
  );
});

test("video parser decodes the Rust v7 frame vector", () => {
  const record = new Uint8Array([
    7, 1, 0, 0, 37, 0, 0, 0, 1, 1, 4, 0, 0, 0, 0, 5, 208, 2, 184, 130, 1, 0, 0,
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

test("video parser rejects an unknown v7 chroma byte", () => {
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
  new Uint8Array(record)[42] = 2;

  assert.equal(parseVideoPacket(record), null);
});
