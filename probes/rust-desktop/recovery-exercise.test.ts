import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { stripTypeScriptTypes } from "node:module";
import test from "node:test";
import vm from "node:vm";
import {
  assessRecoveryExercise,
  parseRecoveryObservation,
  recoveryExerciseTimeoutMs,
} from "./recovery-exercise.js";

interface ProbeObserver {
  start(): void;
  finish(): unknown;
  startRecovery(): unknown;
  recoveryStatus(): unknown;
  finishRecovery(): unknown;
}

async function recoveryHarness() {
  let now = 100;
  let decoderResetCalls = 0;
  const draws: number[] = [];

  class FakeCanvasRenderingContext2D {
    drawImage(frame: { timestamp: number }) {
      draws.push(frame.timestamp);
    }
  }

  class NativeVideoDecoder {
    static readonly marker = "native decoder";
    private queued = 0;
    readonly output: (frame: { timestamp: number }) => void;
    state = "configured";

    constructor(init: { output: (frame: { timestamp: number }) => void }) {
      this.output = init.output;
    }

    get decodeQueueSize() {
      return this.queued;
    }

    reset() {
      decoderResetCalls += 1;
      this.queued = 0;
      this.state = "unconfigured";
    }

    configure() {
      this.state = "configured";
    }

    decode(chunk: { timestamp: number }) {
      this.queued += 1;
      this.output({ timestamp: chunk.timestamp });
      this.queued -= 1;
    }
  }

  class NativeWebSocket {
    static readonly OPEN = 1;
    private readonly listeners: Array<(event: { data: ArrayBuffer }) => void> =
      [];

    constructor(_url: string) {}

    addEventListener(
      type: string,
      listener: (event: { data: ArrayBuffer }) => void,
    ) {
      if (type === "message") this.listeners.push(listener);
    }

    dispatch(data: ArrayBuffer) {
      for (const listener of this.listeners) listener({ data });
    }
  }

  const window = {
    VideoDecoder: NativeVideoDecoder,
    WebSocket: NativeWebSocket,
  };
  const context = {
    window,
    VideoDecoder: NativeVideoDecoder,
    WebSocket: NativeWebSocket,
    CanvasRenderingContext2D: FakeCanvasRenderingContext2D,
    PerformanceObserver: undefined,
    performance: { now: () => now },
    location: { href: "https://desktop.example/" },
    URL,
    ArrayBuffer,
    DataView,
    Object,
    Reflect,
    Infinity,
  };
  const source = await readFile(
    new URL("browser-observer.ts", import.meta.url),
    "utf8",
  );
  vm.runInNewContext(stripTypeScriptTypes(source), context);

  const canvas = new FakeCanvasRenderingContext2D();
  const decoder = new window.VideoDecoder({
    output(frame) {
      canvas.drawImage(frame);
    },
  });
  const socket = new window.WebSocket("wss://desktop.example/stream");
  let waitingForKeyframe = false;
  socket.addEventListener("message", ({ data }) => {
    const view = new DataView(data);
    const flags = view.getUint8(2);
    const keyframe = (flags & 1) !== 0;
    const queuedBeforeDecode = decoder.decodeQueueSize;
    if (queuedBeforeDecode >= 24) {
      decoder.reset();
      decoder.configure();
      waitingForKeyframe = true;
    }
    if (waitingForKeyframe && !keyframe) return;
    waitingForKeyframe = false;
    decoder.decode({ timestamp: Number(view.getBigUint64(12, true)) });
  });

  const observer = (
    window as typeof window & { __rustVideoObserver: ProbeObserver }
  ).__rustVideoObserver;
  observer.start();
  observer.finish();

  let sequence = 0n;
  function frame({
    keyframe = false,
    discontinuity = false,
    generation = 7,
  }: {
    keyframe?: boolean;
    discontinuity?: boolean;
    generation?: number;
  } = {}) {
    sequence += 1n;
    now += 1;
    const packet = new ArrayBuffer(40);
    const view = new DataView(packet);
    view.setUint8(0, 2);
    view.setUint8(1, 1);
    view.setUint8(2, (keyframe ? 1 : 0) | (discontinuity ? 2 : 0));
    view.setBigUint64(4, sequence, true);
    view.setBigUint64(12, sequence * 1_000n, true);
    view.setUint32(20, generation, true);
    view.setBigUint64(28, sequence * 1_000_000n, true);
    socket.dispatch(packet);
  }

  return {
    advance(milliseconds: number) {
      now += milliseconds;
    },
    decoder,
    draws,
    frame,
    get resetCalls() {
      return decoderResetCalls;
    },
    observer,
  };
}

test("one injected queue observation drives the queue-cap reset and captures recovery", async () => {
  const harness = await recoveryHarness();
  harness.observer.startRecovery();

  harness.frame({ keyframe: true });
  assert.equal(harness.resetCalls, 0);
  harness.frame();
  assert.equal(harness.resetCalls, 1);
  assert.equal(harness.decoder.decodeQueueSize, 0);

  harness.frame();
  assert.equal(harness.resetCalls, 1, "the injection must be one-shot");
  harness.frame({ keyframe: true });

  const result = assessRecoveryExercise(harness.observer.finishRecovery());
  assert.equal(result.passed, true);
  assert.equal(result.observation.injection?.actualDecodeQueueSize, 0);
  assert.equal(result.observation.reset?.actualDecodeQueueSize, 0);
  assert.equal(result.observation.resetCount, 1);
  assert.ok(result.observation.firstDecoderOutput);
  assert.ok(result.observation.firstDraw);
  assert.equal(harness.draws.length, 2);
});

test("recovery requires a draw hook and matching output and draw media frames", async () => {
  const harness = await recoveryHarness();
  harness.observer.startRecovery();
  harness.frame({ keyframe: true });
  harness.frame();
  harness.frame({ keyframe: true });
  const observation = parseRecoveryObservation(
    harness.observer.finishRecovery(),
  );
  assert(observation.firstDecoderOutput && observation.firstDraw);
  assert.equal(
    assessRecoveryExercise({ ...observation, drawHookInstalled: false }).passed,
    false,
  );
  for (const field of ["firstDecoderOutput", "firstDraw"] as const) {
    const changed = {
      ...observation,
      [field]: { ...observation[field], mediaTimestampMicros: "999999" },
    };
    assert.equal(assessRecoveryExercise(changed).passed, false);
  }
});

test("the recovery hook expires without injecting after its bounded lifetime", async () => {
  const harness = await recoveryHarness();
  harness.observer.startRecovery();
  harness.frame({ keyframe: true });
  harness.advance(recoveryExerciseTimeoutMs);
  harness.frame();

  const observation = parseRecoveryObservation(
    harness.observer.finishRecovery(),
  );
  assert.equal(observation.timedOut, true);
  assert.equal(observation.injection, null);
  assert.equal(observation.resetCount, 0);
  assert.equal(harness.resetCalls, 0);
  assert.equal(harness.decoder.decodeQueueSize, 0);
});

test("finishing recovery disarms a pending injection and forbids a second attempt", async () => {
  const harness = await recoveryHarness();
  harness.observer.startRecovery();
  harness.frame({ keyframe: true });
  const observation = parseRecoveryObservation(
    harness.observer.finishRecovery(),
  );
  assert.equal(observation.finished, true);

  harness.frame();
  assert.equal(harness.resetCalls, 0);
  assert.equal(harness.decoder.decodeQueueSize, 0);
  assert.throws(() => harness.observer.startRecovery(), /only once/);
});
