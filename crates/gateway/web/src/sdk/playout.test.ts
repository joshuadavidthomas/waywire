import assert from "node:assert/strict";
import test from "node:test";

import {
  initialPlayoutTargetMs,
  Playout,
  type PlayoutSample,
} from "./playout.ts";

const late: PlayoutSample = { latenessMs: 30, decodeQueue: 0 };
const busy: PlayoutSample = { latenessMs: 0, decodeQueue: 5 };
const calm: PlayoutSample = { latenessMs: 10, decodeQueue: 0 };
const mixed: PlayoutSample = { latenessMs: 20, decodeQueue: 2 };

function streak(
  playout: Playout,
  sample: PlayoutSample,
  count: number,
  nowMs: number,
): number {
  let target = initialPlayoutTargetMs;
  for (let index = 0; index < count; index += 1)
    target = playout.update(sample, nowMs);
  return target;
}

test("playout starts at 100 ms and requires four bad frames before raising delay", () => {
  const playout = new Playout();
  assert.equal(playout.update(null, 0), 100);
  assert.equal(streak(playout, late, 3, 10), 100);
  assert.equal(playout.update(late, 20), 125);
});

test("decode backlog still raises delay after four bad samples", () => {
  const playout = new Playout();
  assert.equal(streak(playout, busy, 3, 0), 100);
  assert.equal(playout.update(busy, 16), 125);
});

test("four bad frames two seconds apart recover with equally sparse calm frames", () => {
  const playout = new Playout();
  for (const now of [0, 2_000, 4_000])
    assert.equal(playout.update(late, now), 100);
  assert.equal(playout.update(late, 6_000), 125);
  for (let frame = 1; frame <= 10_000; frame += 1) {
    const now = 6_000 + frame * 2_000;
    const expected =
      now < 16_000 ? 125 : now < 22_000 ? 100 : now < 28_000 ? 75 : 50;
    assert.equal(playout.update(calm, now), expected);
  }
});

test("recovery uses ten seconds since the last bad sample, not calm frame counts", () => {
  for (const intervalMs of [10, 250, 2_000]) {
    const playout = new Playout();
    assert.equal(streak(playout, late, 4, 0), 125);
    for (let now = intervalMs; now < 10_000; now += intervalMs)
      assert.equal(playout.update(calm, now), 125);
    assert.equal(playout.update(calm, 9_999), 125);
    assert.equal(playout.update(calm, 10_000), 100);
  }
  const playout = new Playout();
  assert.equal(streak(playout, calm, 10_000, 0), 100);
});

test("a new bad sample postpones recovery even without enough evidence to raise", () => {
  const playout = new Playout();
  assert.equal(streak(playout, late, 4, 0), 125);
  assert.equal(playout.update(late, 9_000), 125);
  assert.equal(playout.update(null, 10_000), 125);
  assert.equal(playout.update(calm, 18_999), 125);
  assert.equal(playout.update(calm, 19_000), 100);
});

test("idle ticks recover without receiving a single calm frame", () => {
  const playout = new Playout();
  assert.equal(streak(playout, late, 4, 0), 125);
  assert.equal(playout.update(null, 9_999), 125);
  assert.equal(playout.update(null, 10_000), 100);
  assert.equal(playout.update(null, 14_999), 100);
  assert.equal(playout.update(null, 15_000), 75);
  assert.equal(playout.update(null, 20_000), 50);
  assert.equal(playout.update(null, 60_000), 50);
});

test("idle and busy streams drift down on the same wall-clock tick schedule", () => {
  const sparse = new Playout();
  const busyStream = new Playout();
  assert.equal(streak(sparse, late, 4, 0), 125);
  assert.equal(streak(busyStream, late, 4, 0), 125);
  for (let now = 1_000; now <= 20_000; now += 1_000) {
    for (let offset = 16; offset < 1_000; offset += 16)
      busyStream.update(calm, now - 1_000 + offset);
    assert.equal(busyStream.update(null, now), sparse.update(null, now));
  }
  assert.equal(sparse.update(null, 20_000), 50);
});

test("non-bad mixed samples do not postpone recovery", () => {
  const playout = new Playout();
  assert.equal(streak(playout, late, 4, 0), 125);
  assert.equal(playout.update(mixed, 9_000), 125);
  assert.equal(playout.update(mixed, 10_000), 100);
});

test("idle time preserves the bad counter even while the target drifts down", () => {
  const playout = new Playout();
  assert.equal(streak(playout, late, 3, 0), 100);
  assert.equal(playout.update(null, 60_000), 75);
  assert.equal(playout.update(late, 120_000), 100);
});

test("mixed and calm samples still break a bad streak", () => {
  for (const interruption of [mixed, calm]) {
    const playout = new Playout();
    assert.equal(streak(playout, late, 3, 0), 100);
    assert.equal(playout.update(interruption, 10), 100);
    assert.equal(playout.update(late, 20), 100);
  }
});

test("cooldown still spaces upward changes five seconds apart", () => {
  const playout = new Playout();
  assert.equal(streak(playout, late, 4, 0), 125);
  for (let now = 1_000; now <= 4_000; now += 1_000)
    assert.equal(streak(playout, late, 4, now), 125);
  assert.equal(playout.update(late, 4_999), 125);
  assert.equal(playout.update(late, 5_000), 150);
});

test("playout takes 25 ms steps, stops at both bounds, and needs no initial bad frame", () => {
  const quiet = new Playout();
  assert.equal(quiet.update(null, 0), 100);
  assert.equal(quiet.update(null, 10_000), 75);
  assert.equal(quiet.update(null, 15_000), 50);

  const playout = new Playout();
  let now = 0;
  for (let target = 125; target <= 300; target += 25) {
    assert.equal(streak(playout, late, 4, now), target);
    now += 5_000;
  }
  assert.equal(streak(playout, late, 4, now), 300);
  now += 10_000;
  for (let target = 275; target >= 50; target -= 25) {
    assert.equal(playout.update(null, now), target);
    now += 5_000;
  }
  assert.equal(playout.update(null, now), 50);
});

test("normal animation-frame scheduling does not count as bad evidence", () => {
  const playout = new Playout();
  const sample = { latenessMs: 16.9, decodeQueue: 1 };
  assert.equal(playout.update(sample, 0), 100);
  assert.equal(playout.update(sample, 10_000), 75);
});
