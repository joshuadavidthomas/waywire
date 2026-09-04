import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { stripTypeScriptTypes } from "node:module";
import test from "node:test";
import vm from "node:vm";
import {
  decodeFrameCounterCells,
  frameCounterConfiguration,
  frameCounterLavfiInput,
  frameCounterStatus,
  packFrameCounterCells,
  parseFrameCounterTrace,
  summarizeFrameCounter,
} from "./frame-counter.js";

function luminance(frameId: number): number[] {
  return packFrameCounterCells(frameId).map((cell) => (cell ? 235 : 16));
}

function trace(
  records: Array<{
    atMs: number;
    mediaTimestampMicros: number;
    frameId: number;
    status: number;
  }>,
  overflow = 0,
) {
  return parseFrameCounterTrace({
    schema: "sprite-desktop-frame-counter-v1",
    drawHookInstalled: true,
    records,
    overflow,
  });
}

function valid(atMs: number, frameId: number) {
  return {
    atMs,
    mediaTimestampMicros: frameId * 1_000,
    frameId,
    status: frameCounterStatus.valid,
  };
}

test("packs and decodes guards plus 16 value/complement pairs", () => {
  for (const frameId of [0, 1, 0xa55a, 0xffff]) {
    const packed = packFrameCounterCells(frameId);
    assert.equal(packed.length, 36);
    assert.deepEqual(packed.slice(0, 2), [1, 0]);
    assert.deepEqual(packed.slice(-2), [0, 1]);
    for (let bit = 0; bit < 16; bit += 1)
      assert.equal(packed[2 + bit * 2]! + packed[3 + bit * 2]!, 1);
    assert.deepEqual(decodeFrameCounterCells(luminance(frameId)), {
      status: frameCounterStatus.valid,
      frameId,
    });
  }
});

test("rejects damaged complements, guards, contrast, and invalid samples", () => {
  const complement = luminance(7);
  complement[3] = complement[2]!;
  assert.equal(
    decodeFrameCounterCells(complement).status,
    frameCounterStatus.invalidComplement,
  );

  const guard = luminance(7);
  guard[0] = 16;
  assert.equal(
    decodeFrameCounterCells(guard).status,
    frameCounterStatus.invalidGuards,
  );
  assert.equal(
    decodeFrameCounterCells(new Array(36).fill(120)).status,
    frameCounterStatus.invalidGuards,
  );
  assert.equal(
    decodeFrameCounterCells([0, 1]).status,
    frameCounterStatus.unreadableRoi,
  );
  assert.equal(
    decodeFrameCounterCells([...luminance(1).slice(0, 35), Number.NaN]).status,
    frameCounterStatus.unreadableRoi,
  );
});

test("browser draw hook reads only the 36-by-1 counter ROI", async () => {
  let now = 100;
  let counterRead: readonly number[] | undefined;

  class FakeCanvas {
    id = "";
    width = 0;
    height = 0;
    readonly context = new FakeCanvasRenderingContext2D(this);

    getContext(_kind: string, options?: { willReadFrequently?: boolean }) {
      if (this.id !== "display")
        assert.equal(options?.willReadFrequently, true);
      return this.context;
    }
  }
  class FakeCanvasRenderingContext2D {
    private source:
      | { timestamp: number; luminance: readonly number[] }
      | undefined;

    constructor(readonly canvas: FakeCanvas) {}

    drawImage(
      source: { timestamp: number; luminance: readonly number[] },
      ...coordinates: number[]
    ) {
      this.source = source;
      if (this.canvas.id !== "display") {
        assert.deepEqual(coordinates, [0, 0, 288, 8, 0, 0, 36, 1]);
      }
    }

    getImageData(...coordinates: number[]) {
      counterRead = coordinates;
      const data = new Uint8ClampedArray(36 * 4);
      for (let index = 0; index < 36; index += 1) {
        const value = this.source!.luminance[index]!;
        data.set([value, value, value, 255], index * 4);
      }
      return { data };
    }
  }
  class FakeWebSocket {
    addEventListener() {}
  }
  const window = {
    WebSocket: FakeWebSocket,
  };
  const context = {
    window,
    VideoDecoder: undefined,
    WebSocket: FakeWebSocket,
    CanvasRenderingContext2D: FakeCanvasRenderingContext2D,
    PerformanceObserver: undefined,
    performance: { now: () => now },
    document: { createElement: () => new FakeCanvas() },
    location: { href: "https://desktop.example/" },
    URL,
    ArrayBuffer,
    DataView,
    Uint8ClampedArray,
    Object,
    Reflect,
    Infinity,
  };
  const source = await readFile(
    new URL("browser-observer.ts", import.meta.url),
    "utf8",
  );
  vm.runInNewContext(stripTypeScriptTypes(source), context);
  const observer = (
    window as typeof window & {
      __rustVideoObserver: {
        configureFrameCounter(configuration: unknown): void;
        start(): void;
        finish(): unknown;
        finishFrameCounter(): unknown;
      };
    }
  ).__rustVideoObserver;
  observer.configureFrameCounter(frameCounterConfiguration);
  observer.start();

  const display = new FakeCanvas();
  display.id = "display";
  now = 104;
  display.context.drawImage({ timestamp: 6_000, luminance: luminance(1) });
  // Match the recorder's own-property lifetime, not an independent timer.
  const inheritedDraw = display.context.drawImage;
  Object.defineProperty(display.context, "drawImage", {
    configurable: true,
    value: function (this: typeof display.context, ...args: unknown[]) {
      return Reflect.apply(inheritedDraw, this, args);
    },
  });
  now = 105;
  display.context.drawImage({ timestamp: 7_000, luminance: luminance(0xa55a) });
  now = 30_105;
  display.context.drawImage({ timestamp: 8_000, luminance: luminance(0xa55b) });
  Reflect.deleteProperty(display.context, "drawImage");
  now = 30_106;
  display.context.drawImage({ timestamp: 9_000, luminance: luminance(0xa55c) });
  observer.finish();
  const parsed = parseFrameCounterTrace(observer.finishFrameCounter());

  assert.deepEqual(counterRead, [0, 0, 36, 1]);
  assert.deepEqual(JSON.parse(JSON.stringify(parsed.records)), [
    {
      atMs: 5,
      mediaTimestampMicros: 7_000,
      frameId: 0xa55a,
      status: frameCounterStatus.valid,
    },
    {
      atMs: 30_005,
      mediaTimestampMicros: 8_000,
      frameId: 0xa55b,
      status: frameCounterStatus.valid,
    },
  ]);
});

test("lavfi workload overlays every packed cell on the same testsrc2 source", () => {
  const graph = frameCounterLavfiInput();
  assert.match(graph, /^testsrc2=size=1824x848:rate=60,/u);
  assert.match(graph, /w=288:h=8:color=black:t=fill/u);
  assert.equal((graph.match(/drawbox=/gu) ?? []).length, 35);
  assert.match(graph, /enable='bitand\(n\\,32768\)'/u);
  assert.match(graph, /enable='not\(bitand\(n\\,32768\)\)'/u);
});

test("parses explicit invalid and missing samples and rejects inconsistent records", () => {
  const parsed = trace([
    valid(1, 4),
    {
      atMs: 2,
      mediaTimestampMicros: 5_000,
      frameId: -1,
      status: frameCounterStatus.invalidGuards,
    },
    {
      atMs: 3,
      mediaTimestampMicros: -1,
      frameId: -1,
      status: frameCounterStatus.missingTimestamp,
    },
  ]);
  assert.equal(parsed.records.length, 3);

  assert.throws(() =>
    trace([
      {
        atMs: 1,
        mediaTimestampMicros: -1,
        frameId: 9,
        status: frameCounterStatus.missingTimestamp,
      },
    ]),
  );
  assert.throws(() =>
    trace([
      {
        atMs: 1,
        mediaTimestampMicros: 1,
        frameId: -1,
        status: frameCounterStatus.missingTimestamp,
      },
    ]),
  );
});

test("counts distinct IDs, duplicate draws, source gaps, and actual-duration FPS", () => {
  const summary = summarizeFrameCounter(
    trace([valid(1, 100), valid(2, 100), valid(3, 102), valid(4, 103)], 2),
    2_000,
  );
  assert.equal(summary.observedDraws, 6);
  assert.equal(summary.recordedDraws, 4);
  assert.equal(summary.validDraws, 4);
  assert.equal(summary.distinctFrameIds, 3);
  assert.equal(summary.duplicateDraws, 1);
  assert.equal(summary.sourceFrameIdGaps, 1);
  assert.equal(summary.distinctDrawnFps, 1.5);
  assert.equal(summary.overflow, 2);
});
