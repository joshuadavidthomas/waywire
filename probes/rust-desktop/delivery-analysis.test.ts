import assert from "node:assert/strict";
import { test } from "node:test";
import {
  bracketSamples,
  summarizeTcpGap,
  summarizeDurations,
  heartbeatDelayWithinGap,
} from "./delivery-analysis.js";

test("duration summaries retain negative differences and count strict thresholds", () => {
  assert.deepEqual(summarizeDurations([-20, 16, 50, 100, 251]), {
    count: 5,
    p50: 50,
    p95: 251,
    max: 251,
    over50Ms: 2,
    over100Ms: 1,
    over250Ms: 1,
  });
  assert.equal(summarizeDurations([]).max, null);
  assert.throws(() => summarizeDurations([NaN]));
});

test("TCP gap summary preserves separate sockets and absent counters", () => {
  const socket = { recvQueue: 0, sendQueue: 0, counters: {} };
  const before = {
    atPerformanceMs: 50,
    collectionMs: 10,
    sockets: [socket, { ...socket, recvQueue: 10 }],
  };
  const after = { atPerformanceMs: 180, collectionMs: 20, sockets: [socket] };
  const result = summarizeTcpGap([before, after], 100, 120);
  assert.equal(result?.bracketMs, 130);
  assert.equal(result?.before.length, 2);
  assert.equal(result?.after.length, 1);
  assert.equal(result?.before[1]?.recvQueue, 10);
  assert.equal(result?.before[0]?.retrans, null);
  assert.equal(summarizeTcpGap([before], 100, 120), null);
});

test("heartbeat attribution uses the delayed deadline, not the prior timer interval", () => {
  assert.equal(
    heartbeatDelayWithinGap([{ atPerformanceMs: 250, delayMs: 100 }], 100, 120),
    null,
  );
  assert.equal(
    heartbeatDelayWithinGap([{ atPerformanceMs: 250, delayMs: 140 }], 100, 120),
    140,
  );
  assert.equal(
    heartbeatDelayWithinGap([{ atPerformanceMs: 99, delayMs: 50 }], 100, 120),
    null,
  );
  assert.equal(
    heartbeatDelayWithinGap([{ atPerformanceMs: 110, delayMs: 0 }], 100, 120),
    0,
  );
  assert.equal(heartbeatDelayWithinGap([], 100, 120), null);
  assert.throws(() => heartbeatDelayWithinGap([], 120, 100));
});

test("TCP samples bracket the interval outside collection uncertainty", () => {
  const before = { atPerformanceMs: 50, collectionMs: 10 };
  const partial = { atPerformanceMs: 140, collectionMs: 30 };
  const after = { atPerformanceMs: 180, collectionMs: 20 };
  assert.deepEqual(bracketSamples([before, partial, after], 100, 120), {
    before,
    after,
  });
  assert.equal(bracketSamples([partial, after], 100, 120), null);
  assert.equal(bracketSamples([before, partial], 100, 120), null);
});
