import assert from "node:assert/strict";
import test from "node:test";

import { chunkStart } from "./audio.ts";

function assertAbout(actual: number | null, expected: number): void {
  assert.ok(
    actual !== null && Math.abs(actual - expected) < 1e-9,
    `${actual} is not ${expected}`,
  );
}

test("chunks play back to back while they arrive in time", () => {
  assert.equal(chunkStart(10.06, 10), 10.06);
  assert.equal(chunkStart(10.115, 10), 10.115);
});

test("the first chunk and a chunk after a gap start with a short lead", () => {
  assertAbout(chunkStart(0, 10), 10.04);
  assertAbout(chunkStart(10.004, 10), 10.04);
});

test("a chunk that would add lasting delay is dropped", () => {
  assert.equal(chunkStart(10.13, 10), null);
});
