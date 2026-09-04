import assert from "node:assert/strict";
import test from "node:test";

import { WaymoteSession, __testing, type SurfaceOptions } from "./waymote.ts";

class FakeTarget {
  readonly listeners = new Map<string, Set<(event: unknown) => void>>();
  readonly style = {
    cursor: "",
    priority: "",
    getPropertyValue: (): string => this.style.cursor,
    getPropertyPriority: (): string => this.style.priority,
    setProperty: (_name: string, value: string, priority = ""): void => {
      this.style.cursor = value;
      this.style.priority = priority;
    },
    removeProperty: (_name: string): void => {
      this.style.cursor = "";
      this.style.priority = "";
    },
  };
  devicePixelRatio = 1;
  VideoDecoder: unknown = undefined;
  AudioDecoder: unknown = undefined;
  hidden = false;
  pointerLockElement: unknown = null;
  width = 0;
  height = 0;
  rect = { width: 1280, height: 720, left: 0, top: 0 };
  value = "";
  url = "";
  readyState = 0;
  binaryType = "";
  closeCalls = 0;
  getContext: (..._values: unknown[]) => unknown = () => ({});
  requestPointerLock: () => Promise<void> = () => Promise.resolve();
  focus: (..._values: unknown[]) => void = () => undefined;
  getBoundingClientRect = () => ({ ...this.rect });
  hasPointerCapture: (_pointerId: number) => boolean = () => false;
  setPointerCapture: (_pointerId: number) => void = () => undefined;
  releasePointerCapture: (_pointerId: number) => void = () => undefined;

  addEventListener(type: string, listener: (event: unknown) => void): void {
    let listeners = this.listeners.get(type);
    if (!listeners) this.listeners.set(type, (listeners = new Set()));
    listeners.add(listener);
  }
  removeEventListener(type: string, listener: (event: unknown) => void): void {
    this.listeners.get(type)?.delete(listener);
  }
  dispatch(type: string, event: unknown): void {
    for (const listener of this.listeners.get(type) ?? []) listener(event);
  }
  get listenerCount(): number {
    return [...this.listeners.values()].reduce(
      (count, listeners) => count + listeners.size,
      0,
    );
  }
}

function installGlobal(name: string, value: unknown): void {
  Object.defineProperty(globalThis, name, {
    configurable: true,
    writable: true,
    value,
  });
}

function surfaceOptions(
  canvas: FakeTarget,
  textInputElement?: FakeTarget,
): SurfaceOptions {
  if (typeof canvas.getContext !== "function")
    throw new TypeError("fake canvas needs getContext");
  return {
    canvas: canvas as unknown as HTMLCanvasElement,
    ...(textInputElement === undefined
      ? {}
      : { textInputElement: textInputElement as unknown as HTMLInputElement }),
  };
}

function webSocket(socket: FakeTarget | undefined): WebSocket {
  if (!socket || typeof socket.addEventListener !== "function")
    throw new TypeError("fake socket needs listeners");
  return socket as unknown as WebSocket;
}

type SocketAttempt = {
  readonly path: string;
  readonly resolve: (socket: WebSocket) => void;
};

type ScheduledTimer = {
  readonly at: number;
  readonly callback: () => void;
};

type DrawnVideoFrame = {
  readonly timestamp: number;
  readonly at: number;
};

async function createVideoSchedulingHarness(
  options: {
    readonly frameWidth?: number;
    readonly frameHeight?: number;
    readonly remoteDisplay?: import("./waymote.ts").RemoteDisplayPolicy;
    readonly statsIntervalMs?: number;
  } = {},
) {
  const frameWidth = options.frameWidth ?? 1280;
  const frameHeight = options.frameHeight ?? 720;
  let now = 900;
  let nextHandle = 1;
  const timers = new Map<number, ScheduledTimer>();
  const intervals = new Map<number, () => void>();
  const animationFrames = new Map<number, FrameRequestCallback>();

  installGlobal("performance", { now: () => now });
  installGlobal("setTimeout", (callback: TimerHandler, delay = 0): number => {
    if (typeof callback !== "function") {
      throw new TypeError("The scheduling harness requires a callback");
    }
    const handle = nextHandle++;
    timers.set(handle, {
      at: now + Number(delay),
      callback: () => callback(),
    });
    return handle;
  });
  installGlobal("clearTimeout", (handle: number) => timers.delete(handle));
  installGlobal("setInterval", (callback: TimerHandler): number => {
    if (typeof callback !== "function") {
      throw new TypeError("The scheduling harness requires a callback");
    }
    const handle = nextHandle++;
    intervals.set(handle, () => callback());
    return handle;
  });
  installGlobal("clearInterval", (handle: number) => intervals.delete(handle));
  installGlobal(
    "requestAnimationFrame",
    (callback: FrameRequestCallback): number => {
      const handle = nextHandle++;
      animationFrames.set(handle, callback);
      return handle;
    },
  );
  installGlobal("cancelAnimationFrame", (handle: number) => {
    animationFrames.delete(handle);
  });

  const fakeWindow = new FakeTarget();
  fakeWindow.devicePixelRatio = 1;
  fakeWindow.VideoDecoder = true;
  const cursorDraws: Array<{ width: number; height: number }> = [];
  const fakeDocument = new FakeTarget() as FakeTarget & {
    createElement(name: string): unknown;
  };
  fakeDocument.hidden = false;
  fakeDocument.createElement = (name: string) => {
    assert.equal(name, "canvas");
    const cursorCanvas = {
      width: 0,
      height: 0,
      getContext: () => ({
        drawImage: () =>
          cursorDraws.push({
            width: cursorCanvas.width,
            height: cursorCanvas.height,
          }),
      }),
      toDataURL: () => "data:image/png;base64,AQ==",
    };
    return cursorCanvas;
  };
  installGlobal("window", fakeWindow);
  installGlobal("document", fakeDocument);
  installGlobal(
    "Image",
    class {
      src = "";
      readonly naturalWidth = 32;
      readonly naturalHeight = 32;
      decode() {
        return Promise.resolve();
      }
    },
  );
  const resizeCallbacks: Array<() => void> = [];
  installGlobal(
    "ResizeObserver",
    class {
      constructor(callback: () => void) {
        resizeCallbacks.push(callback);
      }
      observe() {}
      disconnect() {}
    },
  );

  class FakeWebSocket extends FakeTarget {
    static CONNECTING = 0;
    static OPEN = 1;
    static CLOSED = 3;
    readyState = FakeWebSocket.CONNECTING;
    binaryType = "";
    readonly sent: unknown[] = [];

    send(value: unknown) {
      this.sent.push(value);
    }

    close() {
      this.readyState = FakeWebSocket.CLOSED;
    }
  }
  installGlobal("WebSocket", FakeWebSocket);

  class FakeVideoFrame {
    readonly displayWidth = frameWidth;
    readonly displayHeight = frameHeight;
    closeCalls = 0;

    constructor(readonly timestamp: number) {}

    close() {
      this.closeCalls += 1;
    }
  }

  type QueuedChunk = { readonly timestamp: number };
  type SchedulingVideoDecoder = {
    delayOutput: boolean;
    readonly decodeQueueSize: number;
    readonly resetCalls: number;
    outputAll(): void;
  };
  const decodedFrames: FakeVideoFrame[] = [];
  let schedulingDecoder: SchedulingVideoDecoder | undefined;
  installGlobal(
    "VideoDecoder",
    class {
      static isConfigSupported() {
        return Promise.resolve({ supported: true });
      }

      state = "unconfigured";
      delayOutput = false;
      resetCalls = 0;
      readonly events = new FakeTarget();
      readonly queuedChunks: QueuedChunk[] = [];
      readonly output: (frame: VideoFrame) => void;

      constructor({ output }: { output: (frame: VideoFrame) => void }) {
        this.output = output;
        schedulingDecoder = this;
      }

      get decodeQueueSize() {
        return this.queuedChunks.length;
      }

      addEventListener(type: string, listener: (event: unknown) => void) {
        this.events.addEventListener(type, listener);
      }

      configure() {
        this.state = "configured";
      }

      reset() {
        this.resetCalls += 1;
        this.queuedChunks.length = 0;
        this.state = "unconfigured";
      }

      close() {
        this.queuedChunks.length = 0;
        this.state = "closed";
      }

      decode(chunk: QueuedChunk) {
        if (this.delayOutput) {
          this.queuedChunks.push(chunk);
          return;
        }
        this.outputChunk(chunk);
      }

      outputAll() {
        const chunks = this.queuedChunks.splice(0);
        this.events.dispatch("dequeue", {});
        for (const chunk of chunks) {
          this.outputChunk(chunk);
        }
      }

      private outputChunk(chunk: QueuedChunk) {
        const frame = new FakeVideoFrame(chunk.timestamp);
        decodedFrames.push(frame);
        this.output(frame as unknown as VideoFrame);
      }
    },
  );
  installGlobal(
    "EncodedVideoChunk",
    class {
      constructor(init: object) {
        Object.assign(this, init);
      }
    },
  );

  const sockets = new Map<string, FakeWebSocket>();
  const draws: DrawnVideoFrame[] = [];
  const canvas = new FakeTarget();
  canvas.width = 1280;
  canvas.height = 720;
  canvas.getContext = () => ({
    drawImage(frame: FakeVideoFrame) {
      draws.push({ timestamp: frame.timestamp, at: now });
    },
  });
  canvas.focus = () => {};
  const textInput = new FakeTarget();
  textInput.focus = () => {};

  const session = new WaymoteSession({
    endpoint: "https://desktop.example.com",
    audio: false,
    ...(options.remoteDisplay ? { remoteDisplay: options.remoteDisplay } : {}),
    ...(options.statsIntervalMs === undefined
      ? {}
      : { statsIntervalMs: options.statsIntervalMs }),
    createWebSocket(path) {
      const socket = new FakeWebSocket();
      sockets.set(path, socket);
      return webSocket(socket);
    },
  });
  session.attachSurface(surfaceOptions(canvas, textInput));
  session.connect();
  await Promise.resolve();
  await Promise.resolve();

  const connectedVideoSocket = sockets.get("/stream");
  const controlSocket = sockets.get("/control");
  if (!connectedVideoSocket || !controlSocket) {
    throw new Error("The scheduling harness did not open its sockets");
  }
  const videoSocket = connectedVideoSocket;
  videoSocket.dispatch("message", {
    data: JSON.stringify({ type: "video-config", codec: "avc1.42E01E" }),
  });
  await new Promise<void>((resolve) => setImmediate(resolve));

  controlSocket.readyState = FakeWebSocket.OPEN;
  const feedback = intervals.values().next().value;
  if (!feedback) {
    throw new Error("The scheduling harness has no feedback timer");
  }
  feedback();
  controlSocket.dispatch("message", {
    data: JSON.stringify({ type: "pong", id: 1, serverNanos: now * 1_000_000 }),
  });
  now += 1;
  feedback();
  controlSocket.dispatch("message", {
    data: JSON.stringify({ type: "pong", id: 2, serverNanos: now * 1_000_000 }),
  });

  async function advanceTo(target: number): Promise<void> {
    if (target < now) {
      throw new RangeError("The test clock cannot move backward");
    }
    while (true) {
      let next: readonly [number, ScheduledTimer] | undefined;
      for (const entry of timers) {
        if (entry[1].at <= target && (!next || entry[1].at < next[1].at)) {
          next = entry;
        }
      }
      if (!next) break;
      timers.delete(next[0]);
      now = next[1].at;
      next[1].callback();
      await Promise.resolve();
    }
    now = target;
  }

  async function runAnimationFrame(at: number): Promise<void> {
    await advanceTo(at);
    const callbacks = [...animationFrames.entries()];
    for (const [handle] of callbacks) animationFrames.delete(handle);
    for (const [, callback] of callbacks) callback(at);
    await Promise.resolve();
  }

  async function emitFrame(
    captureTime: number,
    keyframe = false,
    packetOptions: {
      readonly discontinuity?: boolean;
      readonly generation?: number;
    } = {},
  ) {
    const packet = new ArrayBuffer(41);
    const view = new DataView(packet);
    view.setUint8(0, 2);
    view.setUint8(1, 1);
    view.setUint8(
      2,
      (keyframe ? 1 : 0) | (packetOptions.discontinuity ? 2 : 0),
    );
    view.setBigUint64(12, BigInt(Math.round(captureTime * 1000)), true);
    view.setUint32(20, packetOptions.generation ?? 1, true);
    view.setUint16(24, frameWidth, true);
    view.setUint16(26, frameHeight, true);
    videoSocket.dispatch("message", { data: packet });
    await Promise.resolve();
    await Promise.resolve();
  }

  async function activateCursor(): Promise<void> {
    session.input.acquire();
    const activeControlSocket = controlSocket;
    if (!activeControlSocket) throw new Error("The control socket is missing");
    activeControlSocket.dispatch("message", {
      data: JSON.stringify({ type: "control-state", state: "active" }),
    });
    activeControlSocket.dispatch("message", {
      data: JSON.stringify({
        type: "cursor",
        visible: true,
        width: 32,
        height: 32,
        hotspotX: 4,
        hotspotY: 4,
        image: "data:image/png;base64,AQ==",
      }),
    });
    await Promise.resolve();
    await Promise.resolve();
  }

  const installedDecoder = schedulingDecoder;
  if (!installedDecoder) {
    throw new Error("The scheduling harness has no decoder");
  }

  return {
    activateCursor,
    advanceTo,
    canvas,
    controlMessages: controlSocket.sent,
    cursorDraws,
    decodedFrames,
    decoder: installedDecoder,
    draws,
    emitFrame,
    feedback,
    runAnimationFrame,
    session,
    triggerResizeObservation: () => resizeCallbacks.at(-1)?.(),
  };
}

type ControlledVideoDecoder = {
  readonly decodeQueueSize: number;
  readonly resetCalls: number;
  outputAll(): void;
};

type VideoPacketOptions = {
  readonly keyframe?: boolean;
  readonly discontinuity?: boolean;
  readonly generation?: number;
};

async function createControlledVideoDecoderHarness() {
  let now = 0;
  installGlobal("performance", { now: () => now });
  const intervals: Array<() => void> = [];
  installGlobal("setInterval", (callback: TimerHandler): number => {
    if (typeof callback !== "function")
      throw new TypeError("Expected callback");
    intervals.push(() => callback());
    return intervals.length;
  });
  installGlobal("clearInterval", () => {});
  const fakeWindow = new FakeTarget();
  fakeWindow.devicePixelRatio = 1;
  fakeWindow.VideoDecoder = true;
  const fakeDocument = new FakeTarget();
  fakeDocument.hidden = false;
  installGlobal("window", fakeWindow);
  installGlobal("document", fakeDocument);

  class FakeWebSocket extends FakeTarget {
    static CONNECTING = 0;
    static OPEN = 1;
    static CLOSED = 3;
    readyState = FakeWebSocket.CONNECTING;
    binaryType = "";
    readonly sent: unknown[] = [];

    send(value: unknown) {
      this.sent.push(value);
    }

    close() {
      this.readyState = FakeWebSocket.CLOSED;
    }
  }
  installGlobal("WebSocket", FakeWebSocket);

  class FakeVideoFrame {
    readonly displayWidth = 1280;
    readonly displayHeight = 720;
    closeCalls = 0;

    constructor(readonly timestamp: number) {}

    close() {
      this.closeCalls += 1;
    }
  }

  type QueuedChunk = {
    readonly type: "key" | "delta";
    readonly timestamp: number;
  };

  let controlledDecoder: ControlledVideoDecoder | undefined;
  installGlobal(
    "VideoDecoder",
    class {
      static isConfigSupported() {
        return Promise.resolve({ supported: true });
      }

      state = "unconfigured";
      resetCalls = 0;
      readonly events = new FakeTarget();
      readonly queuedChunks: QueuedChunk[] = [];
      readonly output: (frame: VideoFrame) => void;

      constructor({ output }: { output: (frame: VideoFrame) => void }) {
        this.output = output;
        controlledDecoder = this;
      }

      get decodeQueueSize() {
        return this.queuedChunks.length;
      }

      addEventListener(type: string, listener: (event: unknown) => void) {
        this.events.addEventListener(type, listener);
      }

      configure() {
        this.state = "configured";
      }

      reset() {
        this.resetCalls += 1;
        this.queuedChunks.length = 0;
        this.state = "unconfigured";
      }

      close() {
        this.queuedChunks.length = 0;
        this.state = "closed";
      }

      decode(chunk: QueuedChunk) {
        this.queuedChunks.push(chunk);
      }

      outputAll() {
        const chunks = this.queuedChunks.splice(0);
        this.events.dispatch("dequeue", {});
        for (const chunk of chunks) {
          this.output(
            new FakeVideoFrame(chunk.timestamp) as unknown as VideoFrame,
          );
        }
      }
    },
  );
  installGlobal(
    "EncodedVideoChunk",
    class {
      constructor(init: object) {
        Object.assign(this, init);
      }
    },
  );

  let nextAnimationFrame = 1;
  installGlobal("requestAnimationFrame", (callback: FrameRequestCallback) => {
    const handle = nextAnimationFrame++;
    queueMicrotask(() => callback(performance.now()));
    return handle;
  });
  installGlobal("cancelAnimationFrame", () => {});

  const sockets = new Map<string, FakeWebSocket>();
  const draws: number[] = [];
  const canvas = new FakeTarget();
  canvas.width = 1280;
  canvas.height = 720;
  canvas.getContext = () => ({
    drawImage(frame: FakeVideoFrame) {
      draws.push(frame.timestamp);
    },
  });
  canvas.focus = () => {};
  const textInput = new FakeTarget();
  textInput.focus = () => {};

  const session = new WaymoteSession({
    endpoint: "https://desktop.example.com",
    audio: false,
    createWebSocket(path) {
      const socket = new FakeWebSocket();
      sockets.set(path, socket);
      return webSocket(socket);
    },
  });
  session.attachSurface(surfaceOptions(canvas, textInput));
  session.connect();
  await Promise.resolve();
  await Promise.resolve();

  const connectedVideoSocket = sockets.get("/stream");
  const controlSocket = sockets.get("/control");
  if (!connectedVideoSocket || !controlSocket) {
    throw new Error("The controlled decoder harness has no transport socket");
  }
  const videoSocket = connectedVideoSocket;
  controlSocket.readyState = FakeWebSocket.OPEN;
  videoSocket.dispatch("message", {
    data: JSON.stringify({ type: "video-config", codec: "avc1.42E01E" }),
  });
  await new Promise<void>((resolve) => setImmediate(resolve));
  const installedDecoder = controlledDecoder;
  if (!installedDecoder) {
    throw new Error("The controlled decoder harness has no decoder");
  }
  const decoder = installedDecoder;

  async function emitFrame(
    timestamp: number,
    {
      keyframe = false,
      discontinuity = false,
      generation = 0,
    }: VideoPacketOptions = {},
  ): Promise<void> {
    const packet = new ArrayBuffer(41);
    const view = new DataView(packet);
    view.setUint8(0, 2);
    view.setUint8(1, 1);
    view.setUint8(2, (keyframe ? 1 : 0) | (discontinuity ? 2 : 0));
    view.setBigUint64(12, BigInt(timestamp), true);
    view.setUint32(20, generation, true);
    videoSocket.dispatch("message", { data: packet });
    await Promise.resolve();
    await Promise.resolve();
  }

  async function outputAll(): Promise<void> {
    decoder.outputAll();
    await Promise.resolve();
    await Promise.resolve();
  }

  const feedback = intervals[0];
  if (!feedback)
    throw new Error("The controlled decoder harness has no feedback timer");
  return {
    advance(milliseconds: number) {
      now += milliseconds;
    },
    controlMessages: controlSocket.sent,
    decoder,
    draws,
    emitFrame,
    feedback,
    outputAll,
    session,
  };
}

test("observed resize supports tall displays without exceeding its pixel budget", () => {
  const fitted = __testing.fitObservedResize(1922.5, 2467.5, {
    maxPixels: 2560 * 1440,
  });
  assert.equal(fitted.width % 16, 0);
  assert.equal(fitted.height % 16, 0);
  assert.ok(fitted.width * fitted.height <= 2560 * 1440);
  assert.ok(fitted.height > 1440);
});

test("observed resize accepts the 6000 square protocol envelope", () => {
  assert.deepEqual(
    __testing.fitObservedResize(6000, 6000, { maxPixels: 6000 * 6000 }),
    { width: 6000, height: 6000, downscale: 1 },
  );
});

test("observed resize clamps dimensions above the protocol envelope", () => {
  const fitted = __testing.fitObservedResize(6002, 6002, {
    maxPixels: 6000 * 6000,
  });
  assert.equal(fitted.width, 6000);
  assert.equal(fitted.height, 6000);
  assert.ok(fitted.downscale < 1);
});

test("fixed resize dimensions cannot exceed the protocol envelope", () => {
  assert.deepEqual(__testing.normalizeResizeDimensions(6002, 6002), {
    width: 6000,
    height: 6000,
  });
});

test("double-click stays unlocked; pointer lock requires an explicit request", async () => {
  installGlobal(
    "window",
    Object.assign(new FakeTarget(), { devicePixelRatio: 1 }),
  );
  installGlobal(
    "document",
    Object.assign(new FakeTarget(), {
      hidden: false,
      pointerLockElement: null,
    }),
  );
  let lockRequests = 0;
  const canvas = Object.assign(new FakeTarget(), {
    width: 1280,
    height: 720,
    getContext: () => ({}),
    requestPointerLock: async () => {
      lockRequests++;
    },
    focus() {},
  });
  const textInput = Object.assign(new FakeTarget(), { value: "", focus() {} });
  const session = new WaymoteSession();
  const surface = session.attachSurface(surfaceOptions(canvas, textInput));
  try {
    canvas.dispatch("dblclick", {});
    assert.equal(lockRequests, 0);
    await surface.requestPointerLock();
    assert.equal(lockRequests, 1);
  } finally {
    await session.dispose();
  }
});

test("session disposal is terminal, idempotent, and disposes its surface", async () => {
  const fakeWindow = new FakeTarget();
  fakeWindow.devicePixelRatio = 1;
  const fakeDocument = new FakeTarget();
  fakeDocument.hidden = false;
  fakeDocument.pointerLockElement = null;
  installGlobal("window", fakeWindow);
  installGlobal("document", fakeDocument);

  const canvas = new FakeTarget();
  canvas.width = 1280;
  canvas.height = 720;
  canvas.getContext = () => ({});
  canvas.requestPointerLock = () => Promise.resolve();
  canvas.focus = () => {};
  const textInput = new FakeTarget();
  textInput.value = "";
  textInput.focus = () => {};

  const session = new WaymoteSession();
  const surface = session.attachSurface(surfaceOptions(canvas, textInput));
  assert.ok(canvas.listenerCount > 0);
  assert.ok(textInput.listenerCount > 0);
  assert.ok(fakeDocument.listenerCount > 0);

  const first = session.dispose();
  const second = session.dispose();
  assert.equal(first, second);
  await first;

  assert.equal(canvas.listenerCount, 0);
  assert.equal(textInput.listenerCount, 0);
  assert.equal(fakeDocument.listenerCount, 0);
  assert.throws(() => surface.focus(), /disposed/);
  assert.throws(() => session.connect(), /disposed/);
  assert.throws(
    () => session.attachSurface(surfaceOptions(canvas)),
    /disposed/,
  );
  assert.throws(() => session.on("state", () => {}), /disposed/);
  assert.throws(() => session.input.acquire(), /disposed/);
  assert.throws(() => session.audio.setMuted(true), /disposed/);
  assert.throws(() => session.remoteDisplay.manual(), /disposed/);
  session.disconnect();
});

test("disposal cancels a pending local clipboard paste continuation", async () => {
  const fakeWindow = new FakeTarget();
  fakeWindow.devicePixelRatio = 1;
  const fakeDocument = new FakeTarget();
  fakeDocument.hidden = false;
  fakeDocument.pointerLockElement = null;
  installGlobal("window", fakeWindow);
  installGlobal("document", fakeDocument);

  let resolveClipboard: ((text: string) => void) | undefined;
  const clipboardText = new Promise<string>((resolve) => {
    resolveClipboard = resolve;
  });
  Object.defineProperty(globalThis, "navigator", {
    configurable: true,
    value: { clipboard: { readText: () => clipboardText } },
  });

  const canvas = new FakeTarget();
  canvas.width = 1280;
  canvas.height = 720;
  canvas.getContext = () => ({});
  canvas.requestPointerLock = () => Promise.resolve();
  canvas.focus = () => {};
  const textInput = new FakeTarget();
  textInput.value = "";
  textInput.focus = () => {};

  const session = new WaymoteSession();
  session.attachSurface(surfaceOptions(canvas, textInput));
  const originalWarn = console.warn;
  const warnings: unknown[][] = [];
  console.warn = (...values: unknown[]) => {
    warnings.push(values);
  };
  try {
    canvas.dispatch("keydown", {
      code: "KeyV",
      ctrlKey: true,
      metaKey: false,
      shiftKey: false,
      repeat: false,
      isComposing: false,
      keyCode: 86,
      preventDefault() {},
      stopPropagation() {},
    });
    const disposal = session.dispose();
    resolveClipboard?.("text after disposal");
    await disposal;
    await clipboardText;
    await Promise.resolve();
    await Promise.resolve();
    assert.deepEqual(warnings, []);
  } finally {
    console.warn = originalWarn;
  }
});

test("disconnect invalidates an in-flight audio decoder setup", async () => {
  const fakeWindow = new FakeTarget();
  fakeWindow.devicePixelRatio = 1;
  fakeWindow.VideoDecoder = true;
  fakeWindow.AudioDecoder = true;
  const fakeDocument = new FakeTarget();
  fakeDocument.hidden = false;
  fakeDocument.pointerLockElement = null;
  installGlobal("window", fakeWindow);
  installGlobal("document", fakeDocument);

  class FakeWebSocket extends FakeTarget {
    static CONNECTING = 0;
    static OPEN = 1;
    static CLOSED = 3;
    static sockets: FakeWebSocket[] = [];

    constructor(url: string | URL) {
      super();
      this.url = String(url);
      this.readyState = FakeWebSocket.CONNECTING;
      FakeWebSocket.sockets.push(this);
    }

    send() {}

    close(code = 1000, reason = "") {
      if (this.readyState === FakeWebSocket.CLOSED) return;
      this.readyState = FakeWebSocket.CLOSED;
      this.dispatch("close", { code, reason });
    }
  }
  installGlobal("WebSocket", FakeWebSocket);

  let resolveSupport: ((value: { supported: boolean }) => void) | undefined;
  let supportRequested: (() => void) | undefined;
  const support = new Promise<{ supported: boolean }>((resolve) => {
    resolveSupport = resolve;
  });
  const supportStarted = new Promise<void>((resolve) => {
    supportRequested = resolve;
  });
  let decoderConstructions = 0;
  installGlobal(
    "AudioDecoder",
    class {
      static isConfigSupported() {
        supportRequested?.();
        return support;
      }

      constructor() {
        decoderConstructions += 1;
      }
    },
  );

  class FakeAudioContext {
    state = "running";
    destination = {};
    audioWorklet = { addModule: () => Promise.resolve() };

    createGain() {
      return {
        gain: { value: 1 },
        connect() {
          return this;
        },
        disconnect() {},
      };
    }

    resume() {
      return Promise.resolve();
    }

    close() {
      this.state = "closed";
      return Promise.resolve();
    }
  }
  installGlobal("AudioContext", FakeAudioContext);
  installGlobal(
    "AudioWorkletNode",
    class {
      port = { onmessage: null, postMessage() {} };
      connect(target: unknown): unknown {
        return target;
      }
      disconnect() {}
    },
  );

  const canvas = new FakeTarget();
  canvas.width = 1280;
  canvas.height = 720;
  canvas.getContext = () => ({});
  canvas.requestPointerLock = () => Promise.resolve();
  canvas.focus = () => {};
  const textInput = new FakeTarget();
  textInput.value = "";
  textInput.focus = () => {};

  const session = new WaymoteSession({
    endpoint: "https://desktop.example.com",
    audioWorkletURL: "fake-audio-player.js",
  });
  session.attachSurface(surfaceOptions(canvas, textInput));
  session.connect();
  await Promise.resolve();
  const audioSocket = FakeWebSocket.sockets.find((socket) =>
    socket.url.endsWith("/audio"),
  );
  if (!audioSocket) throw new Error("audio socket was not opened");
  audioSocket.readyState = FakeWebSocket.OPEN;
  audioSocket.dispatch("message", {
    data: JSON.stringify({
      type: "audio-config",
      version: 2,
      enabled: true,
      codec: "opus",
      sampleRate: 48_000,
      channels: 2,
    }),
  });

  const enabling = session.audio.enable();
  await supportStarted;
  const originalWarn = console.warn;
  console.warn = () => {};
  try {
    session.disconnect();
  } finally {
    console.warn = originalWarn;
  }
  resolveSupport?.({ supported: true });
  await enabling;
  assert.equal(decoderConstructions, 0);
  await session.dispose();
});

test("video keyframes wait for decoder setup and reject stale sessions", async () => {
  const fakeWindow = new FakeTarget();
  fakeWindow.devicePixelRatio = 1;
  fakeWindow.VideoDecoder = true;
  const fakeDocument = new FakeTarget();
  fakeDocument.hidden = false;
  installGlobal("window", fakeWindow);
  installGlobal("document", fakeDocument);

  class FakeWebSocket extends FakeTarget {
    static CONNECTING = 0;
    static OPEN = 1;
    static CLOSED = 3;
    readyState = FakeWebSocket.CONNECTING;
    binaryType = "";
    send() {}
    close() {
      this.readyState = FakeWebSocket.CLOSED;
    }
  }
  installGlobal("WebSocket", FakeWebSocket);

  const supportResolvers: Array<(value: { supported: boolean }) => void> = [];
  let decoderConstructions = 0;
  let decodedFrames = 0;
  installGlobal(
    "VideoDecoder",
    class {
      static isConfigSupported() {
        return new Promise((resolve) => supportResolvers.push(resolve));
      }

      state = "unconfigured";
      decodeQueueSize = 0;
      readonly events = new FakeTarget();

      readonly output: (frame: {
        timestamp: number;
        displayWidth: number;
        displayHeight: number;
        close(): void;
      }) => void;
      constructor({
        output,
      }: {
        output: (frame: {
          timestamp: number;
          displayWidth: number;
          displayHeight: number;
          close(): void;
        }) => void;
      }) {
        decoderConstructions += 1;
        this.output = output;
      }

      addEventListener(type: string, listener: (event: unknown) => void) {
        this.events.addEventListener(type, listener);
      }
      configure() {
        this.state = "configured";
      }
      reset() {
        this.state = "unconfigured";
      }
      close() {
        this.state = "closed";
      }
      decode(chunk: { timestamp: number }) {
        decodedFrames += 1;
        this.output({
          timestamp: chunk.timestamp,
          displayWidth: 1280,
          displayHeight: 720,
          close() {},
        });
      }
    },
  );
  installGlobal(
    "EncodedVideoChunk",
    class {
      constructor(init: object) {
        Object.assign(this, init);
      }
    },
  );
  installGlobal("requestAnimationFrame", (callback: FrameRequestCallback) => {
    queueMicrotask(() => callback(performance.now()));
    return 1;
  });
  installGlobal("cancelAnimationFrame", () => {});

  const sockets = new Map<string, FakeWebSocket>();
  const canvas = new FakeTarget();
  canvas.width = 1280;
  canvas.height = 720;
  let renderedFrames = 0;
  canvas.getContext = () => ({
    drawImage() {
      renderedFrames += 1;
    },
  });
  canvas.focus = () => {};
  const textInput = new FakeTarget();
  textInput.value = "";
  textInput.focus = () => {};
  const session = new WaymoteSession({
    endpoint: "https://desktop.example.com",
    audio: false,
    createWebSocket(path) {
      const socket = new FakeWebSocket();
      sockets.set(path, socket);
      return webSocket(socket);
    },
  });
  session.attachSurface(surfaceOptions(canvas, textInput));
  session.connect();
  await Promise.resolve();
  await Promise.resolve();

  const videoSocket = sockets.get("/stream");
  if (!videoSocket) throw new Error("video socket was not opened");
  const keyframe = new ArrayBuffer(41);
  const view = new DataView(keyframe);
  view.setUint8(0, 2);
  view.setUint8(1, 1);
  view.setUint8(2, 1);
  videoSocket.dispatch("message", {
    data: JSON.stringify({ type: "video-config", codec: "avc1.42E01E" }),
  });
  videoSocket.dispatch("message", { data: keyframe });
  assert.equal(decodedFrames, 0);

  supportResolvers.shift()?.({ supported: true });
  await new Promise<void>((resolve) => setImmediate(resolve));
  assert.equal(decodedFrames, 1);
  assert.equal(renderedFrames, 1);

  videoSocket.dispatch("message", {
    data: JSON.stringify({ type: "video-config", codec: "avc1.42E01E" }),
  });
  videoSocket.dispatch("message", { data: keyframe });
  session.disconnect();
  supportResolvers.shift()?.({ supported: true });
  await new Promise<void>((resolve) => setImmediate(resolve));
  assert.equal(decoderConstructions, 1);
  assert.equal(decodedFrames, 1);
  await session.dispose();
});

test("audio-disabled sessions use the socket factory only for video and control", async () => {
  const fakeWindow = new FakeTarget();
  fakeWindow.devicePixelRatio = 1;
  fakeWindow.VideoDecoder = true;
  const fakeDocument = new FakeTarget();
  fakeDocument.hidden = false;
  installGlobal("window", fakeWindow);
  installGlobal("document", fakeDocument);

  class FakeWebSocket extends FakeTarget {
    static CONNECTING = 0;
    static OPEN = 1;
    static CLOSED = 3;
    readyState = FakeWebSocket.CONNECTING;
    binaryType = "";
    send() {}
    close() {
      this.readyState = FakeWebSocket.CLOSED;
    }
  }
  installGlobal("WebSocket", FakeWebSocket);

  const paths: string[] = [];
  const session = new WaymoteSession({
    endpoint: "https://desktop.example.com",
    audio: false,
    createWebSocket(path) {
      paths.push(path);
      return webSocket(new FakeWebSocket());
    },
  });
  const canvas = new FakeTarget();
  canvas.width = 1280;
  canvas.height = 720;
  canvas.getContext = () => ({});
  canvas.focus = () => {};
  const textInput = new FakeTarget();
  textInput.value = "";
  textInput.focus = () => {};
  session.attachSurface(surfaceOptions(canvas, textInput));
  session.connect();
  await Promise.resolve();
  await Promise.resolve();

  assert.deepEqual(paths.sort(), ["/control", "/stream"]);
  assert.equal(session.state.audio.state, "unavailable");
  await session.audio.enable();
  assert.deepEqual(paths.sort(), ["/control", "/stream"]);
  await session.dispose();
});

test("reconnect gets fresh sockets and closes late sockets from the prior connection", async () => {
  const fakeWindow = new FakeTarget();
  fakeWindow.devicePixelRatio = 1;
  fakeWindow.VideoDecoder = true;
  const fakeDocument = new FakeTarget();
  fakeDocument.hidden = false;
  installGlobal("window", fakeWindow);
  installGlobal("document", fakeDocument);

  class FakeWebSocket extends FakeTarget {
    static CONNECTING = 0;
    static OPEN = 1;
    static CLOSED = 3;
    readyState = FakeWebSocket.CONNECTING;
    binaryType = "";
    closeCalls = 0;
    send() {}
    close() {
      this.closeCalls += 1;
      this.readyState = FakeWebSocket.CLOSED;
    }
  }
  installGlobal("WebSocket", FakeWebSocket);

  const attempts: SocketAttempt[] = [];
  const session = new WaymoteSession({
    endpoint: "https://desktop.example.com",
    audio: false,
    createWebSocket(path) {
      let resolve: (socket: WebSocket) => void = () => undefined;
      const promise = new Promise<WebSocket>((next) => {
        resolve = next;
      });
      attempts.push({ path, resolve });
      return promise;
    },
  });
  const canvas = new FakeTarget();
  canvas.width = 1280;
  canvas.height = 720;
  canvas.getContext = () => ({});
  canvas.focus = () => {};
  const textInput = new FakeTarget();
  textInput.value = "";
  textInput.focus = () => {};
  session.attachSurface(surfaceOptions(canvas, textInput));

  session.connect();
  await Promise.resolve();
  assert.equal(attempts.length, 2);
  session.disconnect();
  session.connect();
  await Promise.resolve();
  assert.equal(attempts.length, 4);

  const staleSockets = attempts.slice(0, 2).map(() => new FakeWebSocket());
  attempts
    .slice(0, 2)
    .forEach((attempt, index) =>
      attempt.resolve(webSocket(staleSockets[index])),
    );
  await Promise.resolve();
  await Promise.resolve();
  assert.ok(staleSockets.every((socket) => socket.closeCalls === 1));

  const currentSockets = attempts.slice(2).map(() => new FakeWebSocket());
  attempts
    .slice(2)
    .forEach((attempt, index) =>
      attempt.resolve(webSocket(currentSockets[index])),
    );
  await Promise.resolve();
  await Promise.resolve();
  assert.ok(currentSockets.every((socket) => socket.closeCalls === 0));

  await session.dispose();
  assert.ok(currentSockets.every((socket) => socket.closeCalls === 1));
});

test("visibility changes replace every pending transport socket", async () => {
  const fakeWindow = new FakeTarget();
  fakeWindow.devicePixelRatio = 1;
  fakeWindow.VideoDecoder = true;
  const fakeDocument = new FakeTarget();
  fakeDocument.hidden = false;
  installGlobal("window", fakeWindow);
  installGlobal("document", fakeDocument);

  class FakeWebSocket extends FakeTarget {
    static CONNECTING = 0;
    static OPEN = 1;
    static CLOSED = 3;
    readyState = FakeWebSocket.CONNECTING;
    closeCalls = 0;
    send() {}
    close() {
      this.closeCalls += 1;
      this.readyState = FakeWebSocket.CLOSED;
    }
  }
  installGlobal("WebSocket", FakeWebSocket);

  const attempts: SocketAttempt[] = [];
  const session = new WaymoteSession({
    endpoint: "https://desktop.example.com",
    createWebSocket(path) {
      let resolve: (socket: WebSocket) => void = () => undefined;
      const promise = new Promise<WebSocket>((next) => {
        resolve = next;
      });
      attempts.push({ path, resolve });
      return promise;
    },
  });
  const canvas = new FakeTarget();
  canvas.width = 1280;
  canvas.height = 720;
  canvas.getContext = () => ({});
  canvas.focus = () => {};
  const textInput = new FakeTarget();
  textInput.value = "";
  textInput.focus = () => {};
  session.attachSurface(surfaceOptions(canvas, textInput));

  session.connect();
  await Promise.resolve();
  assert.deepEqual(attempts.map(({ path }) => path).sort(), [
    "/audio",
    "/control",
    "/stream",
  ]);
  fakeDocument.hidden = true;
  fakeDocument.dispatch("visibilitychange", {});
  fakeDocument.hidden = false;
  fakeDocument.dispatch("visibilitychange", {});
  await Promise.resolve();
  assert.equal(attempts.length, 6);

  const staleSockets = attempts.slice(0, 3).map(() => new FakeWebSocket());
  attempts
    .slice(0, 3)
    .forEach((attempt, index) =>
      attempt.resolve(webSocket(staleSockets[index])),
    );
  await Promise.resolve();
  await Promise.resolve();
  assert.ok(staleSockets.every((socket) => socket.closeCalls === 1));

  const currentSockets = attempts.slice(3).map(() => new FakeWebSocket());
  attempts
    .slice(3)
    .forEach((attempt, index) =>
      attempt.resolve(webSocket(currentSockets[index])),
    );
  await Promise.resolve();
  await Promise.resolve();
  assert.ok(currentSockets.every((socket) => socket.closeCalls === 0));

  await session.dispose();
  assert.ok(currentSockets.every((socket) => socket.closeCalls === 1));
});

test("video decoder drains short sequential packet bursts without breaking the delta chain", async () => {
  const harness = await createControlledVideoDecoderHarness();
  try {
    await harness.emitFrame(1_000, { keyframe: true });
    for (let index = 1; index <= 10; index += 1) {
      await harness.emitFrame(1_000 + index);
    }

    assert.equal(harness.decoder.decodeQueueSize, 11);
    assert.equal(harness.decoder.resetCalls, 0);

    await harness.outputAll();
    assert.deepEqual(harness.draws, [1_010]);

    await harness.emitFrame(1_011);
    assert.equal(harness.decoder.decodeQueueSize, 1);
    await harness.outputAll();
    assert.deepEqual(harness.draws, [1_010, 1_011]);
    assert.equal(harness.decoder.resetCalls, 0);
  } finally {
    await harness.session.dispose();
  }
});

test("feedback captures the interval decode queue peak and resets it after the tick", async () => {
  const harness = await createControlledVideoDecoderHarness();
  const latestFeedback = (): Record<string, unknown> | undefined =>
    harness.controlMessages
      .flatMap((value) => {
        if (typeof value !== "string") return [];
        const message = JSON.parse(value) as Record<string, unknown>;
        return message.type === "feedback" ? [message] : [];
      })
      .at(-1);
  try {
    await harness.emitFrame(1_000, { keyframe: true });
    for (let index = 1; index < 5; index += 1) {
      await harness.emitFrame(1_000 + index);
    }
    harness.advance(1000);
    harness.feedback();
    assert.deepEqual(latestFeedback(), {
      type: "feedback",
      received: 5,
      presented: 0,
      queuePeak: 5,
      queueBusyMs: 1000,
      sampleMs: 1000,
      dropped: 0,
      rtt: 0,
    });

    harness.advance(200);
    await harness.outputAll();
    harness.advance(800);
    harness.feedback();
    assert.deepEqual(latestFeedback(), {
      type: "feedback",
      received: 0,
      presented: 1,
      queuePeak: 5,
      queueBusyMs: 200,
      sampleMs: 1000,
      dropped: 4,
      rtt: 0,
    });
    assert.equal(harness.session.stats.receivedFrames, 5);
    assert.equal(harness.session.stats.decodedFrames, 5);
    assert.equal(harness.session.stats.presentedFrames, 1);
    assert.equal(harness.session.stats.overdueDroppedFrames, 4);
  } finally {
    await harness.session.dispose();
  }
});

test("video decoder resets on hard queue overflow and waits for a keyframe", async () => {
  const harness = await createControlledVideoDecoderHarness();
  try {
    await harness.emitFrame(2_000, { keyframe: true });
    for (let index = 1; index < 24; index += 1) {
      await harness.emitFrame(2_000 + index);
    }
    assert.equal(harness.decoder.decodeQueueSize, 24);

    await harness.emitFrame(2_024);
    assert.equal(harness.decoder.resetCalls, 1);
    assert.equal(harness.decoder.decodeQueueSize, 0);

    await harness.emitFrame(2_026);
    assert.equal(harness.decoder.decodeQueueSize, 0);

    await harness.emitFrame(2_027, { keyframe: true });
    await harness.emitFrame(2_028);
    assert.equal(harness.decoder.decodeQueueSize, 2);
    await harness.outputAll();
    assert.deepEqual(harness.draws, [2_028]);
  } finally {
    await harness.session.dispose();
  }
});

test("video discontinuity and generation change share one decoder reset", async () => {
  const harness = await createControlledVideoDecoderHarness();
  try {
    await harness.emitFrame(3_000, { keyframe: true });
    await harness.outputAll();
    assert.deepEqual(harness.draws, [3_000]);

    await harness.emitFrame(3_001, {
      discontinuity: true,
      generation: 1,
    });
    assert.equal(harness.decoder.resetCalls, 1);
    assert.equal(harness.decoder.decodeQueueSize, 0);

    await harness.emitFrame(3_002, { generation: 1 });
    assert.equal(harness.decoder.decodeQueueSize, 0);

    await harness.emitFrame(3_003, { keyframe: true, generation: 1 });
    assert.equal(harness.decoder.decodeQueueSize, 1);
    await harness.outputAll();
    assert.deepEqual(harness.draws, [3_000, 3_003]);
    assert.equal(harness.decoder.resetCalls, 1);
  } finally {
    await harness.session.dispose();
  }
});

test("decoder reset closes scheduled frames before the fresh keyframe is decoded", async () => {
  const harness = await createVideoSchedulingHarness();
  try {
    await harness.advanceTo(1000);
    await harness.emitFrame(1100, true);
    assert.equal(harness.decodedFrames[0]?.closeCalls, 0);

    harness.decoder.delayOutput = true;
    const resetsBefore = harness.decoder.resetCalls;
    await harness.emitFrame(1120, true, {
      discontinuity: true,
      generation: 2,
    });

    assert.equal(harness.decoder.resetCalls, resetsBefore + 1);
    assert.equal(harness.decodedFrames[0]?.closeCalls, 1);
    assert.equal(harness.decoder.decodeQueueSize, 1);

    await harness.advanceTo(1170);
    assert.deepEqual(harness.draws, []);

    harness.decoder.outputAll();
    await harness.runAnimationFrame(1180);
    assert.deepEqual(harness.draws, [{ timestamp: 1_120_000, at: 1180 }]);
    assert.deepEqual(
      harness.decodedFrames.map((frame) => frame.closeCalls),
      [1, 1],
    );
    assert.equal(harness.session.stats.decoderResetDroppedFrames, 1);
  } finally {
    await harness.session.dispose();
  }
});

test("hard decode queue overflow also clears an old scheduled frame", async () => {
  const harness = await createVideoSchedulingHarness();
  try {
    await harness.advanceTo(1000);
    await harness.emitFrame(1100, true);
    harness.decoder.delayOutput = true;

    for (let index = 1; index <= 24; index += 1) {
      await harness.emitFrame(1100 + index);
    }
    assert.equal(harness.decoder.decodeQueueSize, 24);

    const resetsBefore = harness.decoder.resetCalls;
    await harness.emitFrame(1125, true);
    assert.equal(harness.decoder.resetCalls, resetsBefore + 1);
    assert.equal(harness.decodedFrames[0]?.closeCalls, 1);
    assert.equal(harness.decoder.decodeQueueSize, 1);

    await harness.advanceTo(1170);
    assert.deepEqual(harness.draws, []);

    harness.decoder.outputAll();
    await harness.runAnimationFrame(1185);
    assert.deepEqual(harness.draws, [{ timestamp: 1_125_000, at: 1185 }]);
    assert.equal(harness.session.stats.decoderResetDroppedFrames, 25);
  } finally {
    await harness.session.dispose();
  }
});

test("video presentation draws a due frame before a future frame", async () => {
  const harness = await createVideoSchedulingHarness();
  try {
    await harness.advanceTo(1000);
    await harness.emitFrame(935, true);
    await harness.emitFrame(945);

    await harness.runAnimationFrame(1000);

    assert.deepEqual(harness.draws, [{ timestamp: 935_000, at: 1000 }]);
    assert.equal(harness.decodedFrames[0]?.closeCalls, 1);
    assert.equal(harness.decodedFrames[1]?.closeCalls, 0);

    await harness.runAnimationFrame(1005);

    assert.deepEqual(harness.draws, [
      { timestamp: 935_000, at: 1000 },
      { timestamp: 945_000, at: 1005 },
    ]);
    assert.equal(harness.decodedFrames[1]?.closeCalls, 1);
  } finally {
    await harness.session.dispose();
  }
});

test("first non-16:9 frame refreshes an active cursor with ready geometry", async () => {
  const harness = await createVideoSchedulingHarness({
    frameWidth: 1000,
    frameHeight: 1000,
  });
  try {
    harness.canvas.rect = { width: 1600, height: 900, left: 0, top: 0 };
    await harness.activateCursor();
    await harness.advanceTo(1100);
    await harness.emitFrame(1000, true);
    await harness.runAnimationFrame(1100);

    assert.deepEqual(harness.cursorDraws.at(-1), { width: 29, height: 29 });
  } finally {
    await harness.session.dispose();
  }
});

test("ResizeObserver refreshes cursor scale for fixed and manual CSS resizing", async () => {
  for (const remoteDisplay of [
    { mode: "fixed", width: 1000, height: 1000, scale: 1 } as const,
    { mode: "manual" } as const,
  ]) {
    const harness = await createVideoSchedulingHarness({
      frameWidth: 1000,
      frameHeight: 1000,
      remoteDisplay,
    });
    try {
      harness.canvas.rect = { width: 1000, height: 1000, left: 0, top: 0 };
      await harness.activateCursor();
      await harness.advanceTo(1100);
      await harness.emitFrame(1000, true);
      await harness.runAnimationFrame(1100);
      assert.deepEqual(harness.cursorDraws.at(-1), {
        width: 32,
        height: 32,
      });

      harness.canvas.rect = { width: 500, height: 500, left: 0, top: 0 };
      harness.triggerResizeObservation();
      assert.deepEqual(harness.cursorDraws.at(-1), {
        width: 16,
        height: 16,
      });
    } finally {
      await harness.session.dispose();
    }
  }
});

test("video presentation collapses frames that are already overdue", async () => {
  const harness = await createVideoSchedulingHarness();
  try {
    await harness.advanceTo(1100);
    await harness.emitFrame(1010, true);
    await harness.emitFrame(1020);
    await harness.emitFrame(1030);

    await harness.runAnimationFrame(1100);

    assert.deepEqual(harness.draws, [{ timestamp: 1_030_000, at: 1100 }]);
    assert.deepEqual(
      harness.decodedFrames.map((frame) => frame.closeCalls),
      [1, 1, 1],
    );
  } finally {
    await harness.session.dispose();
  }
});

test("decoded output overflow closes frames beyond the 24-frame presentation cap", async () => {
  const harness = await createVideoSchedulingHarness();
  try {
    await harness.advanceTo(1100);
    for (let index = 0; index < 25; index += 1) {
      await harness.emitFrame(1000 + index, index === 0);
    }

    assert.equal(harness.decodedFrames.length, 25);
    assert.equal(harness.decodedFrames[0]?.closeCalls, 1);
    assert.equal(harness.decodedFrames[1]?.closeCalls, 0);
    assert.equal(harness.decodedFrames[24]?.closeCalls, 0);

    await harness.runAnimationFrame(1100);
    assert.equal(harness.session.stats.decodedOverflowDroppedFrames, 1);
    assert.equal(harness.session.stats.droppedFrames, 24);
    assert.ok(harness.decodedFrames.every((frame) => frame.closeCalls === 1));
  } finally {
    await harness.session.dispose();
  }
});

test("feedback reports interval receive and presentation counts with only the new fields", async () => {
  const harness = await createVideoSchedulingHarness();
  const feedbackMessages = (): Array<Record<string, unknown>> =>
    harness.controlMessages.flatMap((value) => {
      if (typeof value !== "string") return [];
      const message = JSON.parse(value) as Record<string, unknown>;
      return message.type === "feedback" ? [message] : [];
    });
  try {
    await harness.advanceTo(1100);
    await harness.emitFrame(1010, true);
    await harness.emitFrame(1020);
    await harness.runAnimationFrame(1100);
    harness.feedback();

    assert.deepEqual(feedbackMessages().at(-1), {
      type: "feedback",
      received: 2,
      presented: 1,
      queuePeak: 0,
      queueBusyMs: 0,
      sampleMs: 199,
      dropped: 1,
      rtt: 0,
    });

    await harness.emitFrame(1030);
    await harness.advanceTo(1101);
    harness.feedback();
    assert.deepEqual(feedbackMessages().at(-1), {
      type: "feedback",
      received: 1,
      presented: 0,
      queuePeak: 0,
      queueBusyMs: 0,
      sampleMs: 1,
      dropped: 0,
      rtt: 0,
    });

    await harness.runAnimationFrame(1102);
    harness.feedback();
    assert.deepEqual(feedbackMessages().at(-1), {
      type: "feedback",
      received: 0,
      presented: 1,
      queuePeak: 0,
      queueBusyMs: 0,
      sampleMs: 1,
      dropped: 0,
      rtt: 0,
    });
  } finally {
    await harness.session.dispose();
  }
});

test("periodic stats skip snapshot work without skipping presentation or counters", async () => {
  const harness = await createVideoSchedulingHarness({ statsIntervalMs: 250 });
  const samples: import("./waymote.ts").WaymoteStats[] = [];
  const remove = harness.session.on("stats", (stats) => samples.push(stats));
  try {
    await harness.advanceTo(1100);
    await harness.emitFrame(1010, true);
    await harness.runAnimationFrame(1100);
    const firstSnapshot = harness.session.stats;

    await harness.emitFrame(1041);
    await harness.runAnimationFrame(1101);
    await harness.emitFrame(1042);
    await harness.runAnimationFrame(1102);

    assert.equal(samples.length, 1);
    assert.equal(harness.session.stats, firstSnapshot);
    assert.equal(harness.draws.length, 3);

    await harness.emitFrame(1290);
    await harness.runAnimationFrame(1350);
    assert.equal(samples.length, 2);
    assert.equal(samples[1]?.receivedFrames, 4);
    assert.equal(samples[1]?.decodedFrames, 4);
    assert.equal(samples[1]?.presentedFrames, 4);
    assert.equal(samples[1]?.renderedMediaTimestampMicros, 1_290_000);
    assert.equal(samples[1]?.drawCompletedAtMs, 1350);
    assert.equal(harness.draws.length, 4);
  } finally {
    remove();
    await harness.session.dispose();
  }
});

test("default stats cadence remains one sample per presented frame", async () => {
  const harness = await createVideoSchedulingHarness();
  const samples: import("./waymote.ts").WaymoteStats[] = [];
  harness.session.on("stats", (stats) => samples.push(stats));
  try {
    await harness.advanceTo(1100);
    await harness.emitFrame(1010, true);
    await harness.runAnimationFrame(1100);
    await harness.emitFrame(1041);
    await harness.runAnimationFrame(1101);
    assert.equal(samples.length, 2);
    assert.deepEqual(
      samples.map(({ renderedMediaTimestampMicros, drawCompletedAtMs }) => ({
        renderedMediaTimestampMicros,
        drawCompletedAtMs,
      })),
      [
        { renderedMediaTimestampMicros: 1_010_000, drawCompletedAtMs: 1100 },
        { renderedMediaTimestampMicros: 1_041_000, drawCompletedAtMs: 1101 },
      ],
    );
    assert.ok(samples.every(Object.isFrozen));
  } finally {
    await harness.session.dispose();
  }
});

test("video presentation catches up a 52.5 Hz capture sequence on 60 Hz animation frames", async () => {
  const harness = await createVideoSchedulingHarness();
  try {
    const displayPeriod = 1000 / 60;
    const capturePeriod = 1000 / 52.5;
    const captures = Array.from(
      { length: 10 },
      (_, index) => 2000 + index * capturePeriod,
    );
    const presentations = captures.map((capture) => capture + 60);
    const arrivals = presentations.map((presentation) => presentation - 20);
    const delayedPresentation = presentations[3];
    if (delayedPresentation === undefined) {
      throw new Error("The capture sequence is too short");
    }
    arrivals[3] = delayedPresentation - 0.2;
    arrivals[4] = arrivals[3] + 0.1;

    const events: Array<
      | {
          readonly type: "capture";
          readonly at: number;
          readonly index: number;
        }
      | { readonly type: "animation"; readonly at: number }
    > = arrivals.map((at, index) => ({ type: "capture", at, index }));
    const lastPresentation = presentations.at(-1);
    if (lastPresentation === undefined) {
      throw new Error("The capture sequence is empty");
    }
    const end = lastPresentation + displayPeriod * 2;
    for (let at = 2000; at <= end; at += displayPeriod) {
      events.push({ type: "animation", at });
    }
    events.sort(
      (left, right) => left.at - right.at || (left.type === "capture" ? -1 : 1),
    );

    for (const event of events) {
      if (event.type === "capture") {
        const capture = captures[event.index];
        if (capture === undefined) {
          throw new Error("The capture event has no timestamp");
        }
        await harness.advanceTo(event.at);
        await harness.emitFrame(capture, event.index === 0);
      } else {
        await harness.runAnimationFrame(event.at);
      }
    }

    assert.deepEqual(
      harness.draws.map((draw) => draw.timestamp),
      captures.map((capture) => Math.round(capture * 1000)),
    );
    for (const draw of harness.draws) {
      const lateness = draw.at - (draw.timestamp / 1000 + 60);
      assert.ok(lateness >= 0, `frame was presented ${-lateness}ms early`);
      assert.ok(
        lateness <= displayPeriod + 0.001,
        `frame was presented ${lateness}ms late`,
      );
    }
  } finally {
    await harness.session.dispose();
  }
});
