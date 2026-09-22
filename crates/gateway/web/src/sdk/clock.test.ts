import assert from "node:assert/strict";
import test from "node:test";
import { ClockSynchronizer } from "./control.ts";

function sample(
  clock: ClockSynchronizer,
  sent: number,
  up: number,
  down: number,
) {
  clock.update(sent, sent + up + down, (sent + up + 1234) * 1e6);
}

test("clock reacquires a sustained higher RTT after confidence expires, requiring two new samples", () => {
  const clock = new ClockSynchronizer();
  sample(clock, 0, 10, 10);
  sample(clock, 1000, 10, 10);
  assert.equal(clock.synchronized(1020), true);
  for (let sent = 2000; sent <= 5000; sent += 1000) sample(clock, sent, 70, 70);
  assert.equal(
    clock.bestRttMilliseconds,
    20,
    "reject high-RTT jitter while the clock is fresh",
  );
  assert.equal(clock.synchronized(6020), true, "inclusive confidence boundary");
  assert.equal(clock.synchronized(6021), false);
  sample(clock, 6000, 70, 70);
  assert.equal(
    clock.synchronized(6140),
    false,
    "one replacement sample is not confidence",
  );
  sample(clock, 7000, 70, 70);
  assert.equal(
    clock.synchronized(7140),
    true,
    "do not wait another 25 seconds with valid samples",
  );
  assert.equal(clock.bestRttMilliseconds, 140);
  assert.equal(clock.offsetMicros, -1234000);
});

test("rejected jitter does not refresh confidence, and a lower RTT can recover the filter", () => {
  const clock = new ClockSynchronizer();
  sample(clock, 0, 30, 30);
  sample(clock, 1000, 30, 30);
  sample(clock, 2000, 30, 130);
  assert.equal(clock.offsetMicros, -1234000);
  assert.equal(clock.synchronized(6060), true);
  assert.equal(clock.synchronized(6061), false);
  sample(clock, 7000, 30, 30);
  assert.equal(clock.synchronized(7060), false);
  sample(clock, 8000, 30, 30);
  assert.equal(clock.synchronized(8060), true);
});

test("one-way asymmetry remains an uncertainty, not a measured one-way delay", () => {
  const clock = new ClockSynchronizer();
  sample(clock, 0, 10, 50);
  sample(clock, 1000, 10, 50);
  assert.equal(clock.bestRttMilliseconds, 60);
  assert.equal(
    clock.offsetMicros,
    -1214000,
    "midpoint has 20ms bias for 10/50 asymmetry",
  );
  assert.equal(clock.synchronized(1060), true);
});

test("invalid stale samples cannot reacquire confidence", () => {
  const clock = new ClockSynchronizer();
  sample(clock, 0, 10, 10);
  sample(clock, 1000, 10, 10);
  clock.update(8000, 7999, "0");
  clock.update(8000, 8010, "NaN");
  clock.update(8000, 68001, "0");
  assert.equal(clock.synchronized(9000), false);
  assert.equal(clock.bestRttMilliseconds, 20);
});
