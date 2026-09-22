import type { SurfaceOptions, WaywireSessionOptions } from "./session.ts";
import { videoFrame } from "./wire.ts";

export class FakeTarget {
  readonly listeners = new Map<string, Set<(event: unknown) => void>>();
  readonly style = {
    cursor: "",
    priority: "",
    display: "",
    transform: "",
    getPropertyValue: (): string => this.style.cursor,
    getPropertyPriority: (): string => this.style.priority,
    setProperty: (_name: string, value: string, priority = ""): void => {
      this.style.cursor = value;
      this.style.priority = priority;
    },
    removeProperty: (): void => {
      this.style.cursor = "";
      this.style.priority = "";
    },
  };
  readonly children: FakeTarget[] = [];
  parent: FakeTarget | null = null;
  get parentElement(): FakeTarget | null {
    return this.parent;
  }
  attributes = new Map<string, string>();
  devicePixelRatio = 1;
  VideoDecoder: unknown = true;
  hidden = false;
  pointerLockElement: unknown = null;
  activeElement: unknown = null;
  width = 1280;
  height = 720;
  rect = { width: 1280, height: 720, left: 0, top: 0 };
  value = "";
  getContext: (...values: unknown[]) => unknown = () => ({});
  requestPointerLock: () => Promise<void> = () => Promise.resolve();
  focus: (...values: unknown[]) => void = () => undefined;
  hasFocus: () => boolean = () => true;
  getBoundingClientRect = () => ({ ...this.rect });
  hasPointerCapture = (_pointerId: number): boolean => false;
  setPointerCapture(_pointerId: number): void {}
  releasePointerCapture(_pointerId: number): void {}

  setAttribute(name: string, value: string): void {
    this.attributes.set(name, value);
  }

  append(child: FakeTarget): void {
    child.remove();
    child.parent = this;
    this.children.push(child);
  }

  contains(target: FakeTarget): boolean {
    return (
      target === this || this.children.some((child) => child.contains(target))
    );
  }

  remove(): void {
    if (!this.parent) return;
    const index = this.parent.children.indexOf(this);
    if (index >= 0) this.parent.children.splice(index, 1);
    this.parent = null;
  }

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

export class FakeWebSocket extends FakeTarget {
  static readonly CONNECTING = 0;
  static readonly OPEN = 1;
  static readonly CLOSED = 3;
  readyState = FakeWebSocket.CONNECTING;
  binaryType = "";
  closeCalls = 0;
  readonly closes: Array<{
    code: number | undefined;
    reason: string | undefined;
  }> = [];
  readonly sent: unknown[] = [];

  send(value: unknown): void {
    this.sent.push(value);
  }

  close(code?: number, reason?: string): void {
    this.closeCalls += 1;
    this.closes.push({ code, reason });
    this.readyState = FakeWebSocket.CLOSED;
  }
}

export function installGlobal(name: string, value: unknown): void {
  Object.defineProperty(globalThis, name, {
    configurable: true,
    writable: true,
    value,
  });
}

export type ResizeObserverHarness = {
  readonly targets: Set<Element>;
  trigger(): void;
};

export function installResizeObserver(): ResizeObserverHarness[] {
  const observers: ResizeObserverHarness[] = [];
  installGlobal(
    "ResizeObserver",
    class {
      readonly targets = new Set<Element>();
      private initialCallbackScheduled = false;
      constructor(private readonly callback: ResizeObserverCallback) {
        observers.push({
          targets: this.targets,
          trigger: () => this.deliver(),
        });
      }
      private deliver(): void {
        const entries = [...this.targets].flatMap((target) => {
          const contentRect = target.getBoundingClientRect();
          return contentRect.width > 0 && contentRect.height > 0
            ? [{ target, contentRect } as ResizeObserverEntry]
            : [];
        });
        if (entries.length > 0) {
          this.callback(entries, this as unknown as ResizeObserver);
        }
      }
      observe(target: Element): void {
        this.targets.add(target);
        if (this.initialCallbackScheduled) return;
        this.initialCallbackScheduled = true;
        queueMicrotask(() => {
          this.initialCallbackScheduled = false;
          this.deliver();
        });
      }
      unobserve(target: Element): void {
        this.targets.delete(target);
      }
      disconnect(): void {
        this.targets.clear();
      }
    },
  );
  return observers;
}

export function installBrowser(): {
  window: FakeTarget;
  document: FakeTarget;
} {
  const fakeWindow = new FakeTarget();
  const fakeDocument = new FakeTarget();
  const body = new FakeTarget();
  Object.assign(fakeDocument, {
    body,
    createElement: () => new FakeTarget(),
  });
  installGlobal("window", fakeWindow);
  installGlobal("document", fakeDocument);
  installGlobal("WebSocket", FakeWebSocket);
  installGlobal("requestAnimationFrame", (callback: FrameRequestCallback) => {
    queueMicrotask(() => callback(performance.now()));
    return 1;
  });
  installGlobal("cancelAnimationFrame", () => undefined);
  return { window: fakeWindow, document: fakeDocument };
}

export function surfaceOptions(
  canvas = new FakeTarget(),
  textInputElement = new FakeTarget(),
): SurfaceOptions {
  return {
    canvas: canvas as unknown as HTMLCanvasElement,
    textInputElement: textInputElement as unknown as HTMLInputElement,
  };
}

export function socket(socket: FakeWebSocket): WebSocket {
  return socket as unknown as WebSocket;
}

export type SocketAttempt = {
  readonly path: string;
  readonly resolve: (socket: WebSocket) => void;
};

export function deferredSocketOptions(
  attempts: SocketAttempt[],
): WaywireSessionOptions {
  return {
    endpoint: "https://desktop.example.com",
    createWebSocket(path) {
      let resolve: (value: WebSocket) => void = () => undefined;
      const pending = new Promise<WebSocket>((next) => {
        resolve = next;
      });
      attempts.push({ path, resolve });
      return pending;
    },
  };
}

export function installEncodedVideoChunk(): void {
  installGlobal(
    "EncodedVideoChunk",
    class {
      constructor(init: object) {
        Object.assign(this, init);
      }
    },
  );
}

export function videoPacket(
  timestamp: number,
  options: {
    readonly keyframe?: boolean;
    readonly discontinuity?: boolean;
    readonly generation?: number;
    readonly width?: number;
    readonly height?: number;
    readonly chroma?: 0 | 1;
  } = {},
): ArrayBuffer {
  return videoFrame(
    options.keyframe ? 1 : 0,
    options.discontinuity ? 1 : 0,
    {
      generation: options.generation ?? 1,
      width: options.width ?? 1280,
      height: options.height ?? 720,
      captureNanos: BigInt(timestamp) * 1000n,
      sequence: 0n,
      inputSequence: 0,
      fps: 60,
      chroma: options.chroma ?? 0,
    },
    new Uint8Array([0]),
  );
}

export type QueueDecoder = {
  readonly decodeQueueSize: number;
  readonly resetCalls: number;
  outputAll(): void;
};

export function installQueueVideoDecoder(): {
  readonly decoder: () => QueueDecoder | undefined;
} {
  let decoder: QueueDecoder | undefined;
  class FakeVideoFrame {
    readonly displayWidth = 1280;
    readonly displayHeight = 720;
    constructor(readonly timestamp: number) {}
    close(): void {}
  }
  installGlobal(
    "VideoDecoder",
    class {
      static isConfigSupported() {
        return Promise.resolve({ supported: true });
      }
      state = "unconfigured";
      resetCalls = 0;
      readonly queued: Array<{ timestamp: number }> = [];
      readonly events = new FakeTarget();
      constructor(readonly init: { output(frame: VideoFrame): void }) {
        decoder = this;
      }
      get decodeQueueSize() {
        return this.queued.length;
      }
      addEventListener(type: string, listener: (event: unknown) => void) {
        this.events.addEventListener(type, listener);
      }
      configure() {
        this.state = "configured";
      }
      reset() {
        this.resetCalls += 1;
        this.queued.length = 0;
        this.state = "unconfigured";
      }
      close() {
        this.queued.length = 0;
        this.state = "closed";
      }
      decode(chunk: { timestamp: number }) {
        this.queued.push(chunk);
      }
      outputAll() {
        const chunks = this.queued.splice(0);
        this.events.dispatch("dequeue", {});
        for (const chunk of chunks) {
          this.init.output(
            new FakeVideoFrame(chunk.timestamp) as unknown as VideoFrame,
          );
        }
      }
    },
  );
  installEncodedVideoChunk();
  return { decoder: () => decoder };
}

export function installDelayedVideoDecoder(): {
  readonly supportResolvers: Array<(value: { supported: boolean }) => void>;
  readonly counts: { constructions: number; decoded: number };
  readonly configurations: VideoDecoderConfig[];
} {
  const supportResolvers: Array<(value: { supported: boolean }) => void> = [];
  const counts = { constructions: 0, decoded: 0 };
  const configurations: VideoDecoderConfig[] = [];
  installGlobal(
    "VideoDecoder",
    class {
      static isConfigSupported() {
        return new Promise((resolve) => supportResolvers.push(resolve));
      }
      state = "unconfigured";
      decodeQueueSize = 0;
      readonly events = new FakeTarget();
      constructor(
        readonly init: {
          output(frame: VideoFrame): void;
        },
      ) {
        counts.constructions += 1;
      }
      addEventListener(type: string, listener: (event: unknown) => void) {
        this.events.addEventListener(type, listener);
      }
      configure(configuration: VideoDecoderConfig) {
        configurations.push(configuration);
        this.state = "configured";
      }
      reset() {
        this.state = "unconfigured";
      }
      close() {
        this.state = "closed";
      }
      decode(chunk: { timestamp: number }) {
        counts.decoded += 1;
        this.init.output({
          timestamp: chunk.timestamp,
          displayWidth: 1280,
          displayHeight: 720,
          close() {},
        } as VideoFrame);
      }
    },
  );
  installEncodedVideoChunk();
  return { supportResolvers, counts, configurations };
}

export async function flush(): Promise<void> {
  await Promise.resolve();
  await Promise.resolve();
}
