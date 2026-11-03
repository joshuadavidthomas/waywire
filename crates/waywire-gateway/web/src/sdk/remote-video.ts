import type { ClockState, ClockSynchronizer } from "./clock.ts";
import { ProtocolVersionMismatchError } from "./messages.ts";
import type { VideoState, WaywireStats } from "./session.ts";
import { TrackSink } from "./track-sink.ts";
import {
  latencyMilliseconds,
  videoOptions,
  VideoRuntime,
  type VideoFeedback,
  type VideoOwner,
  type VideoPipeline,
} from "./video.ts";

/** The page state the video's connection logic reads, sent with every command. */
export type RunState = Readonly<{
  shouldRun: boolean;
  generation: number;
  disposed: boolean;
}>;

type Command =
  | Readonly<{
      type: "init";
      url: string;
      latency: number;
      statsIntervalMs: number;
      timeOrigin: number;
    }>
  | Readonly<{
      type: "attach";
      canvas: OffscreenCanvas;
      writable: WritableStream<VideoFrame> | null;
    }>
  | Readonly<{ type: "detach" }>
  | Readonly<{ type: "connect" }>
  | Readonly<{ type: "disconnect" }>
  | Readonly<{ type: "invalidate" }>
  | Readonly<{ type: "hide" }>
  | Readonly<{ type: "latency"; milliseconds: number }>
  | Readonly<{ type: "reset-feedback" }>
  | Readonly<{ type: "feedback" }>
  | Readonly<{ type: "clock"; state: ClockState }>
  | Readonly<{ type: "showing" }>
  | Readonly<{ type: "fullscreen"; fullscreen: boolean }>;

export type PageMessage = Command & Readonly<{ run: RunState }>;

export type WorkerMessage =
  | Readonly<{ type: "ready"; supported: boolean; timeOrigin: number }>
  | Readonly<{ type: "status"; text: string; connected: boolean | undefined }>
  | Readonly<{ type: "state"; changes: Partial<VideoState> }>
  | Readonly<{ type: "error"; name: string; message: string }>
  | Readonly<{
      type: "halt";
      message: string;
      mismatch: Readonly<{ expected: number; actual: number }> | null;
    }>
  | Readonly<{ type: "input"; sequence: number }>
  | Readonly<{
      type: "presented";
      generation: number;
      width: number;
      height: number;
    }>
  | Readonly<{ type: "stats"; stats: WaywireStats }>
  | Readonly<{
      type: "feedback";
      feedback: VideoFeedback | null;
      stats: Partial<WaywireStats>;
    }>
  | Readonly<{ type: "key-frame" }>;

type Pending = Readonly<{
  command: Command;
  transfer: Transferable[];
  local: (video: VideoRuntime) => void;
}>;

/**
 * Whether the page can run the video in a worker: it has workers and can hand
 * a canvas to one. A session that makes its own WebSockets keeps the video on
 * the page, since a worker cannot call the page's factory. The worker checks
 * the rest (WebCodecs, OffscreenCanvas, animation frames) when it starts.
 */
export function videoWorkerAvailable(customSockets: boolean): boolean {
  return (
    !customSockets &&
    typeof Worker === "function" &&
    typeof OffscreenCanvas === "function" &&
    typeof HTMLCanvasElement === "function" &&
    "transferControlToOffscreen" in HTMLCanvasElement.prototype
  );
}

const transferredCanvases = new WeakSet<HTMLCanvasElement>();

function transferCanvas(canvas: HTMLCanvasElement): OffscreenCanvas {
  if (transferredCanvases.has(canvas)) {
    throw new Error(
      "This canvas was already attached, and the video worker keeps it; attach a new canvas",
    );
  }
  try {
    const offscreen = canvas.transferControlToOffscreen();
    transferredCanvases.add(canvas);
    return offscreen;
  } catch (cause) {
    throw new Error(
      "The attached canvas already has a rendering context; the video worker needs a canvas without one",
      { cause },
    );
  }
}

/**
 * Runs the session's video in a dedicated worker, so a busy page does not
 * hold back decoding or presentation. The page keeps what needs the DOM: the
 * `<video>` and its track where Chromium has `MediaStreamTrackGenerator`, the
 * fullscreen state and the session's state. If the worker cannot start or
 * lacks a feature, the same playout runs on the page instead.
 */
export class RemoteVideo implements VideoPipeline {
  private worker: Worker | null = null;
  private local: VideoRuntime | null = null;
  private ready = false;
  private readonly pending: Pending[] = [];
  private readonly feedbackRequests: Array<
    (feedback: VideoFeedback | null) => void
  > = [];
  /** How much later the worker's clock started than the page's. */
  private behindMilliseconds = 0;
  private presentedFrames = 0;
  private lastStats: Partial<WaywireStats> = {};
  private canvas: HTMLCanvasElement | null = null;
  private sink: TrackSink | null = null;
  private stopWatchingFullscreen: (() => void) | null = null;
  private readonly latency: number;
  private readonly statsIntervalMs: number;

  constructor(
    private readonly owner: VideoOwner,
    private readonly clock: ClockSynchronizer,
    url: URL,
    latency: number | undefined,
    statsIntervalMs: number | undefined,
  ) {
    const options = videoOptions(latency, statsIntervalMs);
    this.latency = options.latency;
    this.statsIntervalMs = options.statsIntervalMs;
    let worker: Worker;
    try {
      worker = new Worker(new URL("./video-worker.ts", import.meta.url), {
        type: "module",
        name: "waywire-video",
      });
    } catch (error) {
      console.warn("video worker did not start; playing on the page", error);
      this.fail();
      return;
    }
    this.worker = worker;
    worker.addEventListener("message", (event: MessageEvent<WorkerMessage>) =>
      this.receive(event.data),
    );
    worker.addEventListener("error", (event) => {
      if (this.worker !== worker) return;
      if (this.ready) {
        console.error("video worker error", event.message);
        this.owner.setStatus("Video worker error");
        return;
      }
      console.warn("video worker did not start; playing on the page");
      this.fail();
    });
    worker.postMessage({
      type: "init",
      url: url.href,
      latency: this.latency,
      statsIntervalMs: this.statsIntervalMs,
      timeOrigin: performance.timeOrigin,
      run: this.run(),
    } satisfies PageMessage);
    clock.onUpdate = () =>
      this.send({ type: "clock", state: clock.state }, () => undefined);
  }

  get hasSocket(): boolean {
    // The worker connects only when it has no socket, so the page can always ask.
    return this.local?.hasSocket ?? false;
  }

  get renderedFrameCount(): number {
    return this.local?.renderedFrameCount ?? this.presentedFrames;
  }

  get stats(): Readonly<Partial<WaywireStats>> {
    if (this.local) return this.local.stats;
    return Object.freeze(this.onPageClock(this.lastStats));
  }

  /** Hand the canvas to the worker for good, with the `<video>` track where there is one. */
  attach(canvas: HTMLCanvasElement): void {
    const offscreen = transferCanvas(canvas);
    this.canvas = canvas;
    const sink = TrackSink.over(canvas);
    this.sink = sink;
    sink?.onShowing(() => {
      if (this.sink === sink) this.send({ type: "showing" }, () => undefined);
    });
    const fullscreen = () =>
      this.send(
        {
          type: "fullscreen",
          fullscreen: document.fullscreenElement === canvas,
        },
        () => undefined,
      );
    document.addEventListener("fullscreenchange", fullscreen);
    this.stopWatchingFullscreen = () =>
      document.removeEventListener("fullscreenchange", fullscreen);
    fullscreen();
    this.send(
      { type: "attach", canvas: offscreen, writable: sink?.writable ?? null },
      (video) => {
        const context = offscreen.getContext("2d", { alpha: false });
        if (context) video.attach(offscreen, context);
      },
      sink ? [offscreen, sink.writable] : [offscreen],
    );
  }

  detach(): void {
    this.stopWatchingFullscreen?.();
    this.stopWatchingFullscreen = null;
    this.send({ type: "detach" }, (video) => video.detach());
    this.sink?.close();
    this.sink = null;
    this.canvas = null;
  }

  connect(): Promise<void> {
    let connected = Promise.resolve();
    this.send({ type: "connect" }, (video) => {
      connected = video.connect();
    });
    return connected;
  }

  disconnect(): void {
    this.send({ type: "disconnect" }, (video) => video.disconnect());
  }

  invalidateConnectionAttempt(): void {
    this.send({ type: "invalidate" }, (video) =>
      video.invalidateConnectionAttempt(),
    );
  }

  closeForHiddenPage(): void {
    this.send({ type: "hide" }, (video) => video.closeForHiddenPage());
  }

  setLatencyTarget(milliseconds: number): void {
    const target = latencyMilliseconds(milliseconds);
    this.send({ type: "latency", milliseconds: target }, (video) =>
      video.setLatencyTarget(target),
    );
  }

  resetFeedbackInterval(): void {
    this.send({ type: "reset-feedback" }, (video) =>
      video.resetFeedbackInterval(),
    );
  }

  requestFeedback(deliver: (feedback: VideoFeedback | null) => void): void {
    if (this.local) {
      this.local.requestFeedback(deliver);
    } else if (this.ready) {
      this.feedbackRequests.push(deliver);
      this.send({ type: "feedback" }, () => undefined);
    } else {
      deliver(null);
    }
  }

  private run(): RunState {
    return {
      shouldRun: this.owner.shouldRun(),
      generation: this.owner.connectionGeneration(),
      disposed: this.owner.disposed(),
    };
  }

  private send(
    command: Command,
    local: (video: VideoRuntime) => void,
    transfer: Transferable[] = [],
  ): void {
    if (this.local) {
      local(this.local);
    } else if (!this.ready) {
      this.pending.push({ command, transfer, local });
    } else {
      this.worker?.postMessage(
        { ...command, run: this.run() } satisfies PageMessage,
        transfer,
      );
    }
  }

  /** Play on the page, replaying what the session asked of the worker so far. */
  private fail(): void {
    this.worker?.terminate();
    this.worker = null;
    this.ready = false;
    this.clock.onUpdate = null;
    const local = new VideoRuntime(
      {
        ...this.owner,
        createTrackSink: () => this.sink,
        displayFullscreen: () =>
          this.canvas !== null && document.fullscreenElement === this.canvas,
      },
      this.latency,
      this.statsIntervalMs,
    );
    this.local = local;
    for (const { local: replay } of this.pending.splice(0)) replay(local);
  }

  private onPageClock(stats: Partial<WaywireStats>): Partial<WaywireStats> {
    const merged = { ...stats, ...this.owner.statsContext() };
    if (stats.drawCompletedAtMs !== undefined) {
      merged.drawCompletedAtMs =
        stats.drawCompletedAtMs + this.behindMilliseconds;
    }
    return merged;
  }

  private receive(message: WorkerMessage): void {
    switch (message.type) {
      case "ready":
        if (!message.supported) {
          console.warn("video worker lacks a feature; playing on the page");
          this.fail();
          return;
        }
        this.ready = true;
        this.behindMilliseconds = message.timeOrigin - performance.timeOrigin;
        this.send({ type: "clock", state: this.clock.state }, () => undefined);
        for (const { command, transfer } of this.pending.splice(0)) {
          this.send(command, () => undefined, transfer);
        }
        return;
      case "status":
        this.owner.setStatus(message.text, message.connected);
        return;
      case "state":
        this.owner.updateState(message.changes);
        return;
      case "error":
        this.owner.emitError(
          Object.assign(new Error(message.message), { name: message.name }),
        );
        return;
      case "halt":
        this.owner.halt(
          message.mismatch
            ? new ProtocolVersionMismatchError(
                message.mismatch.expected,
                message.mismatch.actual,
              )
            : new Error(message.message),
        );
        return;
      case "input":
        this.owner.setLatestAppliedInput(message.sequence);
        return;
      case "presented":
        this.presentedFrames += 1;
        this.owner.presentResizeGeneration(
          message.generation,
          message.width,
          message.height,
        );
        return;
      case "stats":
        this.lastStats = message.stats;
        this.owner.publishStats(
          Object.freeze(this.onPageClock(message.stats)) as WaywireStats,
        );
        return;
      case "feedback":
        this.lastStats = { ...this.lastStats, ...message.stats };
        this.feedbackRequests.shift()?.(message.feedback);
        return;
      case "key-frame":
        this.owner.requestKeyFrame();
        return;
    }
  }
}
