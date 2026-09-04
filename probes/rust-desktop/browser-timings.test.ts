import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { stripTypeScriptTypes } from "node:module";
import test from "node:test";
import vm from "node:vm";
import {
  parseBrowserTimingTrace,
  summarizeBrowserTimings,
} from "./browser-timings.js";

const fixture = () => ({
  schema: "sprite-desktop-browser-timings-v2",
  recordingPerformanceStartMs: 100,
  headers: [
    {
      atMs: 10,
      sequence: "7",
      generation: 2,
      mediaTimestampMicros: "1000",
      captureNanos: "900000",
      flags: 1,
      auBytes: 300,
    },
    {
      atMs: 20,
      sequence: "8",
      generation: 2,
      mediaTimestampMicros: "2000",
      captureNanos: "1900000",
      flags: 0,
      auBytes: 250,
    },
  ],
  decodeSubmissions: [
    { atMs: 11, mediaTimestampMicros: "1000", auBytes: 300 },
    { atMs: 22, mediaTimestampMicros: "2000", auBytes: 250 },
  ],
  decoderOutputs: [
    { atMs: 14, mediaTimestampMicros: "1000" },
    { atMs: 26, mediaTimestampMicros: "2000" },
  ],
  resets: [] as { atMs: number; queued: number }[],
  longTasks: [{ atMs: 15, durationMs: 55 }],
  overflow: {
    headers: 0,
    decodeSubmissions: 0,
    decoderOutputs: 0,
    resets: 0,
    longTasks: 0,
  },
});

test("observer wrappers preserve WebCodecs construction and static behavior", async () => {
  let now = 100;
  class NativeVideoDecoder {
    static readonly marker = "native static";
    static isConfigSupported(value: string) {
      return `supported:${value}`;
    }
    readonly output: (frame: { timestamp: number }) => void;
    decodeQueueSize = 0;

    constructor(init: { output: (frame: { timestamp: number }) => void }) {
      this.output = init.output;
    }

    reset() {}
    decode() {}
  }
  class NativeWebSocket {
    static readonly OPEN = 1;
    addEventListener() {}
  }
  const window = {
    VideoDecoder: NativeVideoDecoder,
    WebSocket: NativeWebSocket,
  };
  const context = {
    window,
    VideoDecoder: NativeVideoDecoder,
    WebSocket: NativeWebSocket,
    PerformanceObserver: undefined,
    performance: { now: () => now },
    location: { href: "https://desktop.example/" },
    URL,
    ArrayBuffer,
    DataView,
    Infinity,
  };
  const source = await readFile(
    new URL("browser-observer.ts", import.meta.url),
    "utf8",
  );
  vm.runInNewContext(stripTypeScriptTypes(source), context);

  const Instrumented = window.VideoDecoder;
  class ChildDecoder extends Instrumented {}
  const outputTimestamps: number[] = [];
  const decoder = new ChildDecoder({
    output: (frame) => outputTimestamps.push(frame.timestamp),
  });
  assert.equal(Instrumented.marker, "native static");
  assert.equal(Instrumented.isConfigSupported("avc"), "supported:avc");
  assert.ok(decoder instanceof ChildDecoder);
  assert.ok(decoder instanceof NativeVideoDecoder);

  const observer = (
    window as typeof window & {
      __rustVideoObserver: {
        start(): void;
        finish(): unknown;
      };
    }
  ).__rustVideoObserver;
  observer.start();
  now = 105;
  decoder.output({ timestamp: 1234 });
  assert.deepEqual(outputTimestamps, [1234]);
  const parsed = parseBrowserTimingTrace(observer.finish());
  assert.deepEqual(JSON.parse(JSON.stringify(parsed)), {
    schema: "sprite-desktop-browser-timings-v2",
    recordingPerformanceStartMs: 100,
    headers: [],
    decodeSubmissions: [],
    decoderOutputs: [{ atMs: 5, mediaTimestampMicros: "1234" }],
    resets: [],
    longTasks: [],
    overflow: {
      headers: 0,
      decodeSubmissions: 0,
      decoderOutputs: 0,
      resets: 0,
      longTasks: 0,
    },
  });
});

test("strictly parses and correlates browser timing boundaries", () => {
  const trace = parseBrowserTimingTrace(fixture());
  const summary = summarizeBrowserTimings(trace, [
    { renderedMediaTimestampMicros: 1000, drawCompletedAtMs: 115 },
    { renderedMediaTimestampMicros: 2000, drawCompletedAtMs: 130 },
    { renderedMediaTimestampMicros: 3000, drawCompletedAtMs: 131 },
  ]);
  assert.equal(summary.receiveToDecodeSubmissionMs.p50, 1);
  assert.equal(summary.receiveToDecodeSubmissionMs.max, 2);
  assert.equal(summary.decodeSubmissionToOutputMs.p50, 3);
  assert.equal(summary.decodeSubmissionToOutputMs.max, 4);
  assert.equal(summary.decoderOutputToDrawMs.p50, 1);
  assert.equal(summary.decoderOutputToDrawMs.max, 4);
  assert.equal(summary.counts.exactMatchedDraws, 2);
  assert.equal(summary.counts.decoderOutputsNotDrawn, 0);
  assert.equal(summary.counts.recorderDrawsWithoutDecoderOutput, 1);
  assert.equal(summary.auBytes.count, 2);
  assert.equal(summary.longTaskDurationMs.p95, 55);
});

test("rejects unknown, secret-shaped, malformed, and out-of-window fields", () => {
  assert.throws(() => parseBrowserTimingTrace({ ...fixture(), url: "secret" }));
  const payload = fixture();
  payload.headers[0] = { ...payload.headers[0]!, payload: "encoded" } as never;
  assert.throws(() => parseBrowserTimingTrace(payload));

  const badTimestamp = fixture();
  badTimestamp.decoderOutputs[0]!.mediaTimestampMicros = "01";
  assert.throws(() => parseBrowserTimingTrace(badTimestamp));

  const late = fixture();
  late.longTasks[0]!.atMs = 30_000;
  assert.throws(() => parseBrowserTimingTrace(late));
});

test("surfaces each bounded-array overflow and rejects oversized arrays", () => {
  const value = fixture();
  value.overflow.decoderOutputs = 4;
  assert.equal(parseBrowserTimingTrace(value).overflow.decoderOutputs, 4);

  const oversized = fixture();
  oversized.resets = new Array(1_001).fill({ atMs: 1, queued: 0 });
  assert.throws(() => parseBrowserTimingTrace(oversized));
});

test("rejects mismatched AU sizes, duplicate keys, and negative spans", () => {
  const size = parseBrowserTimingTrace(fixture());
  const changed = { ...size.decodeSubmissions[0]!, auBytes: 301 };
  assert.throws(() =>
    summarizeBrowserTimings(
      {
        ...size,
        decodeSubmissions: [changed, ...size.decodeSubmissions.slice(1)],
      },
      [],
    ),
  );

  assert.throws(() =>
    summarizeBrowserTimings(
      {
        ...size,
        decoderOutputs: [size.decoderOutputs[0]!, size.decoderOutputs[0]!],
      },
      [],
    ),
  );

  const early = { ...size.decoderOutputs[0]!, atMs: 9 };
  assert.throws(() =>
    summarizeBrowserTimings(
      { ...size, decoderOutputs: [early, ...size.decoderOutputs.slice(1)] },
      [],
    ),
  );

  assert.throws(() =>
    summarizeBrowserTimings(size, [
      { renderedMediaTimestampMicros: 1000, drawCompletedAtMs: 113 },
    ]),
  );
  assert.throws(() =>
    summarizeBrowserTimings(size, [
      { renderedMediaTimestampMicros: 1000, drawCompletedAtMs: 115 },
      { renderedMediaTimestampMicros: 1000, drawCompletedAtMs: 116 },
    ]),
  );
});
