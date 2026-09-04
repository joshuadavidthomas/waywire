import assert from "node:assert/strict";
import type { TcpSamplingSnapshot } from "./tcp-sampling.js";

export function summarizeDurations(values: readonly number[]) {
  assert(values.every(Number.isFinite));
  const sorted = values.toSorted((a, b) => a - b);
  return {
    count: sorted.length,
    p50: sorted[Math.ceil(sorted.length * 0.5) - 1] ?? null,
    p95: sorted[Math.ceil(sorted.length * 0.95) - 1] ?? null,
    max: sorted.at(-1) ?? null,
    over50Ms: sorted.filter((value) => value > 50).length,
    over100Ms: sorted.filter((value) => value > 100).length,
    over250Ms: sorted.filter((value) => value > 250).length,
  };
}

export function summarizeTcpGap(
  samples: TcpSamplingSnapshot["samples"],
  start: number,
  end: number,
) {
  const bracket = bracketSamples(samples, start, end);
  if (!bracket) return null;
  const sockets = (sample: typeof bracket.before) =>
    sample.sockets.map((socket) => ({
      recvQueue: socket.recvQueue,
      bytesReceived: socket.counters.bytes_received ?? null,
      outOfOrder: socket.counters.rcv_ooopack ?? null,
      retrans: socket.counters.retrans ?? null,
    }));
  return {
    bracketMs: bracket.after.atPerformanceMs - bracket.before.atPerformanceMs,
    before: sockets(bracket.before),
    after: sockets(bracket.after),
  };
}

export function heartbeatDelayWithinGap(
  beats: readonly { atPerformanceMs: number; delayMs: number }[],
  start: number,
  end: number,
): number | null {
  assert(Number.isFinite(start) && Number.isFinite(end) && end >= start);
  const delays = beats
    .filter((beat) => {
      assert(
        Number.isFinite(beat.atPerformanceMs) &&
          Number.isFinite(beat.delayMs) &&
          beat.delayMs >= 0,
      );
      const deadline = beat.atPerformanceMs - beat.delayMs;
      return beat.atPerformanceMs > start && deadline <= end;
    })
    .map((beat) => beat.delayMs);
  return delays.length ? Math.max(...delays) : null;
}

// Bracket the whole interval, including the uncertainty while ss was running.
export function bracketSamples<
  T extends { atPerformanceMs: number; collectionMs: number },
>(
  samples: readonly T[],
  start: number,
  end: number,
): { before: T; after: T } | null {
  const before = samples.findLast((sample) => sample.atPerformanceMs <= start);
  const after = samples.find(
    (sample) => sample.atPerformanceMs - sample.collectionMs >= end,
  );
  return before && after ? { before, after } : null;
}
