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

test("sustained decode backlog raises delay even when frames are on time", () => {
  const playout = new Playout();
  assert.equal(streak(playout, busy, 3, 0), 100);
  assert.equal(playout.update(busy, 16), 125);
});

test("lowering delay needs a much longer calm streak", () => {
  const playout = new Playout();
  assert.equal(streak(playout, calm, 119, 0), 100);
  assert.equal(playout.update(calm, 16), 75);
});

test("cooldown spaces changes five seconds apart while collecting streaks", () => {
  const playout = new Playout();
  assert.equal(streak(playout, late, 4, 0), 125);
  for (let now = 1_000; now <= 4_000; now += 1_000)
    assert.equal(streak(playout, late, 4, now), 125);
  assert.equal(playout.update(late, 4_999), 125);
  assert.equal(playout.update(late, 5_000), 150);
});

test("playout takes 25 ms steps and stops at both bounds", () => {
  const playout = new Playout();
  let now = 0;
  for (let target = 125; target <= 300; target += 25) {
    assert.equal(streak(playout, late, 4, now), target);
    now += 5_000;
  }
  assert.equal(streak(playout, late, 4, now), 300);
  for (let target = 275; target >= 50; target -= 25) {
    now += 5_000;
    assert.equal(streak(playout, calm, 120, now), target);
  }
  now += 5_000;
  assert.equal(streak(playout, calm, 120, now), 50);
});

test("mixed, opposite, untimed, and idle samples break streaks", () => {
  for (const interruption of [mixed, calm, null]) {
    const playout = new Playout();
    assert.equal(streak(playout, late, 3, 0), 100);
    assert.equal(playout.update(interruption, 10), 100);
    assert.equal(playout.update(late, 20), 100);
  }
  const playout = new Playout();
  assert.equal(streak(playout, late, 3, 0), 100);
  assert.equal(playout.update(late, 1_501), 100);
  assert.equal(streak(playout, calm, 119, 1_502), 100);
  assert.equal(playout.update(calm, 3_003), 100);
});

test("normal animation-frame scheduling does not raise delay", () => {
  const playout = new Playout();
  assert.equal(
    streak(playout, { latenessMs: 16.9, decodeQueue: 1 }, 119, 0),
    100,
  );
  assert.equal(playout.update({ latenessMs: 16.9, decodeQueue: 1 }, 16), 75);
});
