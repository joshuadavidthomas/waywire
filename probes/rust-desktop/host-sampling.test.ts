import assert from "node:assert/strict";
import test from "node:test";
import { setTimeout as sleep } from "node:timers/promises";
import { hostSamplingTesting, startHostSampling } from "./host-sampling.js";

function procStat(
  pid: number,
  comm: string,
  options: {
    ppid?: number;
    userTicks?: number;
    systemTicks?: number;
    startTicks?: number;
  } = {},
): string {
  const fields = [
    options.ppid ?? 1,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    0,
    options.userTicks ?? 0,
    options.systemTicks ?? 0,
    0,
    0,
    0,
    0,
    1,
    0,
    options.startTicks ?? 100,
    4096,
    10,
  ];
  return `${pid} (${comm}) S ${fields.join(" ")}\n`;
}

test("parses proc stat comm fields containing spaces and parentheses", () => {
  const parsed = hostSamplingTesting.parseProcStat(
    procStat(42, "worker (pool) 1", {
      ppid: 7,
      userTicks: 20,
      systemTicks: 3,
      startTicks: 999,
    }),
  );
  assert.deepEqual(parsed, {
    pid: 42,
    comm: "worker (pool) 1",
    ppid: 7,
    cpuTicks: 23n,
    startTicks: 999n,
    rssPages: 10n,
  });
});

test("accepts only numeric pid and ppid process rows", () => {
  assert.deepEqual(hostSamplingTesting.parseProcessTree(" 10  1\n11 10\n"), [
    { pid: 10, ppid: 1 },
    { pid: 11, ppid: 10 },
  ]);
  assert.throws(() =>
    hostSamplingTesting.parseProcessTree("10 1 browser --token=secret\n"),
  );
  assert.throws(() => hostSamplingTesting.parseProcessTree("10 1\n10 2\n"));
});

test("finds only descendants rooted at the supplied PID", () => {
  const rows = hostSamplingTesting.parseProcessTree(
    "1 0\n10 1\n11 10\n12 11\n20 1\n21 20\n",
  );
  assert.deepEqual(hostSamplingTesting.descendants(rows, 10), [
    { pid: 10, ppid: 1 },
    { pid: 11, ppid: 10 },
    { pid: 12, ppid: 11 },
  ]);
});

test("CPU is one-core percent and a reused PID starts a new span", () => {
  const before = hostSamplingTesting.parseProcStat(
    procStat(50, "thread", { userTicks: 100, startTicks: 500 }),
  );
  const after = hostSamplingTesting.parseProcStat(
    procStat(50, "thread", { userTicks: 125, startTicks: 500 }),
  );
  const reused = hostSamplingTesting.parseProcStat(
    procStat(50, "thread", { userTicks: 900, startTicks: 700 }),
  );
  assert.equal(
    hostSamplingTesting.statCpuPercent(after, before, 1_000, 100),
    25,
  );
  assert.equal(
    hostSamplingTesting.statCpuPercent(reused, before, 1_000, 100),
    null,
  );
});

test("finish cancels sampling and returns bounded performance-clock heartbeats", async () => {
  const sampling = await startHostSampling(process.pid);
  await sleep(1_150);
  const snapshot = await sampling.finish();
  const again = await sampling.finish();
  assert.strictEqual(again, snapshot);
  assert(snapshot.startPerformanceMs <= snapshot.finishPerformanceMs);
  assert(snapshot.samples.length >= 1);
  assert(snapshot.samples.length <= 40);
  assert(snapshot.samples[0]!.processes.some(({ pid }) => pid === process.pid));
  assert(snapshot.samples[0]!.topThreads.length <= 12);
  assert(snapshot.heartbeat.length >= 1);
  assert(snapshot.heartbeat.length <= 600);
  assert(
    snapshot.heartbeat.every(
      ({ atPerformanceMs, delayMs }) =>
        atPerformanceMs >= snapshot.startPerformanceMs &&
        Number.isFinite(delayMs) &&
        delayMs >= 0,
    ),
  );
});
