// The session's video in a dedicated worker: the stream socket, decoding,
// playout scheduling and drawing, the same VideoRuntime the page runs when it
// has no worker. See remote-video.ts for the page's side.
import { ClockSynchronizer } from "./clock.ts";
import { ProtocolVersionMismatchError } from "./messages.ts";
import type { PageMessage, RunState, WorkerMessage } from "./remote-video.ts";
import type { FrameSink } from "./track-sink.ts";
import { VideoRuntime, type VideoOwner } from "./video.ts";

type WorkerScope = {
  postMessage(message: WorkerMessage): void;
  addEventListener(
    type: "message",
    listener: (event: MessageEvent<PageMessage>) => void,
  ): void;
  close(): void;
};
const scope = globalThis as unknown as WorkerScope;
const post = (message: WorkerMessage) => scope.postMessage(message);

const supported =
  typeof VideoDecoder === "function" &&
  typeof OffscreenCanvas === "function" &&
  typeof requestAnimationFrame === "function" &&
  typeof WebSocket === "function";

/** Writes frames to the page's `<video>` track through its transferred stream. */
class TransferredTrackSink implements FrameSink {
  showing = false;
  private readonly writer: WritableStreamDefaultWriter<VideoFrame>;

  constructor(writable: WritableStream<VideoFrame>) {
    this.writer = writable.getWriter();
  }

  show(frame: VideoFrame): void {
    this.writer.write(frame.clone()).catch(() => undefined);
  }

  close(): void {
    this.writer.close().catch(() => undefined);
  }
}

let run: RunState = { shouldRun: false, generation: 0, disposed: false };
let url = "";
let behindMilliseconds = 0;
let writable: WritableStream<VideoFrame> | null = null;
let sink: TransferredTrackSink | null = null;
let fullscreen = false;
let appliedInput: number | null = null;
let video: VideoRuntime | null = null;
const clock = new ClockSynchronizer();

const owner: VideoOwner = {
  createSocket: async () => new WebSocket(url),
  connectionGeneration: () => run.generation,
  shouldRun: () => run.shouldRun,
  disposed: () => run.disposed,
  setStatus: (text, connected) => post({ type: "status", text, connected }),
  updateState: (changes) => post({ type: "state", changes }),
  emitError: (error) =>
    post({ type: "error", name: error.name, message: error.message }),
  halt: (error) =>
    post({
      type: "halt",
      message: error.message,
      mismatch:
        error instanceof ProtocolVersionMismatchError
          ? { expected: error.expected, actual: error.actual }
          : null,
    }),
  expectedPresentationTime: (captureMicros, latencyMilliseconds) =>
    clock.expectedPresentationTime(captureMicros, latencyMilliseconds),
  // The page replaces everything but the clock's confidence with its own.
  statsContext: () => ({
    bitrateKbps: 0,
    scalePercent: 0,
    rttMs: 0,
    clockConfident: clock.synchronized(),
    clockUncertaintyMs: null,
    pendingInputCount: 0,
    resizeState: "idle",
  }),
  setLatestAppliedInput: (sequence) => {
    if (sequence === appliedInput) return;
    appliedInput = sequence;
    post({ type: "input", sequence });
  },
  presentResizeGeneration: (generation, width, height) =>
    post({ type: "presented", generation, width, height }),
  publishStats: (stats) => post({ type: "stats", stats }),
  requestKeyFrame: () => post({ type: "key-frame" }),
  createTrackSink: () => {
    sink = writable ? new TransferredTrackSink(writable) : null;
    writable = null;
    return sink;
  },
  displayFullscreen: () => fullscreen,
};

scope.addEventListener("message", ({ data: message }) => {
  run = message.run;
  if (message.type === "init") {
    url = message.url;
    behindMilliseconds = performance.timeOrigin - message.timeOrigin;
    if (supported) {
      video = new VideoRuntime(owner, message.latency, message.statsIntervalMs);
    }
    post({ type: "ready", supported, timeOrigin: performance.timeOrigin });
    return;
  }
  if (!video) return;
  switch (message.type) {
    case "attach": {
      const context = message.canvas.getContext("2d", { alpha: false });
      if (!context) throw new Error("The video worker has no 2D canvas");
      video.attach(message.canvas, context);
      writable = message.writable;
      return;
    }
    case "detach":
      video.detach();
      writable = null;
      sink = null;
      return;
    case "connect":
      if (!video.hasSocket) void video.connect();
      return;
    case "disconnect":
      video.disconnect();
      if (run.disposed) scope.close();
      return;
    case "invalidate":
      video.invalidateConnectionAttempt();
      return;
    case "hide":
      video.closeForHiddenPage();
      return;
    case "latency":
      video.setLatencyTarget(message.milliseconds);
      return;
    case "reset-feedback":
      video.resetFeedbackInterval();
      return;
    case "feedback":
      post({
        type: "feedback",
        feedback: video.takeFeedback(),
        stats: video.stats,
      });
      return;
    case "clock":
      clock.follow(message.state, behindMilliseconds);
      return;
    case "showing":
      if (sink) sink.showing = true;
      return;
    case "fullscreen":
      fullscreen = message.fullscreen;
      return;
  }
});
