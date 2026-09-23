import {
  PROTOCOL_VERSION,
  ProtocolVersionMismatchError,
  parseVideoConfiguration,
  parseJson,
  type VideoConfiguration,
} from "./messages.ts";
import type { WaywireStats } from "./session.ts";
import { decodeRecord, readFrameMetadata } from "./wire.ts";
export const maximumVideoDecodeQueueSize = 24;
const busyDecodeQueueSize = 5;
const initialReconnectDelayMilliseconds = 250;
const maximumReconnectDelayMilliseconds = 5000;
const presentationLeadMilliseconds = 8;
const presentationWakeMarginMilliseconds = 4;
const maximumPresentationTimerMilliseconds = 1000;

type Timer = ReturnType<typeof setTimeout>;
type ConnectionAttempt = Readonly<{ generation: number }>;

export type VideoFeedback = Readonly<{
  received: number;
  presented: number;
  queuePeak: number;
  queueBusyMs: number;
  sampleMs: number;
  /** Capacity/reset loss, excluding frames superseded by a newer canvas draw. */
  dropped: number;
}>;

export type VideoStatsContext = Readonly<{
  bitrateKbps: number;
  scalePercent: number;
  rttMs: number;
  clockConfident: boolean;
  clockUncertaintyMs: number | null;
  pendingInputCount: number;
  resizeState: WaywireStats["resizeState"];
}>;

export interface VideoOwner {
  createSocket(): Promise<WebSocket>;
  connectionGeneration(): number;
  shouldRun(): boolean;
  disposed(): boolean;
  setStatus(text: string, connected?: boolean): void;
  updateState(changes: Partial<import("./session.ts").VideoState>): void;
  emitError(error: Error): void;
  halt(error: Error): void;
  expectedPresentationTime(
    captureMicros: number,
    latencyMilliseconds: number,
  ): number | null;
  statsContext(): VideoStatsContext;
  setLatestAppliedInput(sequence: number): void;
  presentResizeGeneration(
    generation: number,
    width: number,
    height: number,
  ): void;
  publishStats(stats: WaywireStats): void;
}

export type VideoPacket = Readonly<{
  data: Uint8Array;
  keyframe: boolean;
  discontinuity: boolean;
  timestamp: number;
  generation: number;
  latestAppliedInput: number;
  width: number;
  height: number;
  fps: number;
}>;

export function parseVideoPacket(buffer: ArrayBuffer): VideoPacket | null {
  try {
    const { kind, payload } = decodeRecord(buffer);
    if (kind !== 1) return null;
    const frameKind = payload.u8();
    const continuity = payload.u8();
    if (
      (frameKind !== 0 && frameKind !== 1) ||
      (continuity !== 0 && continuity !== 1)
    ) {
      return null;
    }
    const metadata = readFrameMetadata(payload);
    const data = payload.rest();
    payload.finish();
    return {
      data,
      keyframe: frameKind === 1,
      discontinuity: continuity === 1,
      timestamp: Number(metadata.captureNanos / 1000n),
      generation: metadata.generation,
      latestAppliedInput: metadata.inputSequence,
      width: metadata.width,
      height: metadata.height,
      fps: metadata.fps,
    };
  } catch {
    return null;
  }
}

type FrameMetadata = Pick<
  VideoPacket,
  "generation" | "width" | "height" | "fps"
>;
type PendingVideoFrame = Readonly<{
  frame: VideoFrame;
  metadata: FrameMetadata;
}>;

export class VideoRuntime {
  private display: HTMLCanvasElement | null = null;
  private context: CanvasRenderingContext2D | null = null;
  private decoder: VideoDecoder | null = null;
  private socket: WebSocket | null = null;
  private readonly pendingFrames: PendingVideoFrame[] = [];
  // WebCodecs returns the chunk timestamp with each decoded frame. Keep packet
  // metadata until that output arrives, including repeated timestamps.
  private readonly decodingFrames = new Map<number, FrameMetadata[]>();
  private animationPending = false;
  private animationFrame: number | null = null;
  private presentationTimer: Timer | null = null;
  private renderedFrames = 0;
  private presentedFrames = 0;
  private intervalPresentedFrames = 0;
  private decodedFrames = 0;
  private reconnectDelay = initialReconnectDelayMilliseconds;
  private reconnectTimer: Timer | null = null;
  private connectAttempt: ConnectionAttempt | null = null;
  private decoderConfiguration: VideoDecoderConfig | null = null;
  private waitingForKeyframe = true;
  private decoderGeneration = 0;
  private receivedChunks = 0;
  private intervalReceivedFrames = 0;
  private receivedKeyframes = 0;
  private queuePeak = 0;
  private queueBusyMilliseconds = 0;
  private queueObservedSize = 0;
  private queueObservedAt = 0;
  private feedbackSampleStartedAt = 0;
  private readonly renderedFrameTimes: number[] = [];
  private targetLatencyMilliseconds: number;
  private videoLateness = 0;
  private readonly statsIntervalMilliseconds: number;
  private lastStatsPublishedAt = Number.NEGATIVE_INFINITY;
  private currentGeneration = 0;
  private droppedFrames = 0;
  private intervalDroppedFrames = 0;
  private overdueDroppedFrames = 0;
  private decodedOverflowDroppedFrames = 0;
  private decoderResetDroppedFrames = 0;
  private decoderResets = 0;
  private lastDrawStats: WaywireStats | null = null;

  constructor(
    private readonly owner: VideoOwner,
    latency: number | undefined,
    statsIntervalMs: number | undefined,
  ) {
    this.targetLatencyMilliseconds = Number(latency ?? 60);
    this.statsIntervalMilliseconds = Number(statsIntervalMs ?? 0);
    if (
      !Number.isFinite(this.targetLatencyMilliseconds) ||
      this.targetLatencyMilliseconds < 0
    ) {
      throw new RangeError("Latency must be a non-negative number");
    }
    if (
      !Number.isFinite(this.statsIntervalMilliseconds) ||
      this.statsIntervalMilliseconds < 0
    ) {
      throw new RangeError("Stats interval must be a non-negative number");
    }
  }

  get hasSocket(): boolean {
    return this.socket !== null;
  }

  get renderedFrameCount(): number {
    return this.renderedFrames;
  }

  // Polling diagnostics must still work when no frame is being drawn. Events
  // remain draw-linked so callers never mistake an idle sample for a new frame.
  get stats(): Readonly<Partial<WaywireStats>> {
    return Object.freeze({ ...this.lastDrawStats, ...this.diagnostics() });
  }

  private diagnostics(now = performance.now()) {
    while ((this.renderedFrameTimes[0] ?? now) < now - 1000) {
      this.renderedFrameTimes.shift();
    }
    const elapsed = now - (this.renderedFrameTimes[0] ?? now);
    return {
      ...this.owner.statsContext(),
      renderedFps:
        elapsed > 0
          ? ((this.renderedFrameTimes.length - 1) * 1000) / elapsed
          : 0,
      latencyTargetMs: this.targetLatencyMilliseconds,
      decoderQueue: this.decoder?.decodeQueueSize ?? 0,
      pendingVideoFrames: this.pendingFrames.length,
      receivedFrames: this.receivedChunks,
      decodedFrames: this.decodedFrames,
      presentedFrames: this.presentedFrames,
      droppedFrames: this.droppedFrames,
      overdueDroppedFrames: this.overdueDroppedFrames,
      decodedOverflowDroppedFrames: this.decodedOverflowDroppedFrames,
      decoderResetDroppedFrames: this.decoderResetDroppedFrames,
      decoderResets: this.decoderResets,
    };
  }

  attach(display: HTMLCanvasElement, context: CanvasRenderingContext2D): void {
    this.display = display;
    this.context = context;
  }

  detach(): void {
    this.display = null;
    this.context = null;
  }

  invalidateConnectionAttempt(): void {
    this.connectAttempt = null;
  }

  closeForHiddenPage(): void {
    this.connectAttempt = null;
    this.socket?.close(1000, "page hidden");
  }

  async connect(): Promise<void> {
    if (!this.owner.shouldRun()) return;
    if (this.reconnectTimer !== null) {
      clearTimeout(this.reconnectTimer);
      this.reconnectTimer = null;
    }
    if (this.connectAttempt) return;
    this.owner.setStatus("Connecting");
    const attempt = { generation: this.owner.connectionGeneration() };
    this.connectAttempt = attempt;
    let socket: WebSocket;
    try {
      socket = await this.owner.createSocket();
    } catch (cause) {
      if (this.connectAttempt !== attempt) return;
      this.connectAttempt = null;
      if (
        !this.owner.shouldRun() ||
        attempt.generation !== this.owner.connectionGeneration()
      )
        return;
      const error = cause instanceof Error ? cause : new Error(String(cause));
      this.owner.emitError(error);
      this.owner.setStatus(`Reconnecting · ${error.message}`);
      this.reconnectTimer = setTimeout(() => {
        this.reconnectTimer = null;
        void this.connect();
      }, this.reconnectDelay);
      this.reconnectDelay = Math.min(
        this.reconnectDelay * 2,
        maximumReconnectDelayMilliseconds,
      );
      return;
    }
    if (
      this.connectAttempt !== attempt ||
      !this.owner.shouldRun() ||
      attempt.generation !== this.owner.connectionGeneration()
    ) {
      if (this.connectAttempt === attempt) this.connectAttempt = null;
      socket.close(1000, "stale connection attempt");
      return;
    }
    this.connectAttempt = null;
    this.socket = socket;
    let decoderSetup = Promise.resolve();
    socket.binaryType = "arraybuffer";
    socket.addEventListener("open", () => {
      if (this.socket !== socket) return;
      this.reconnectDelay = initialReconnectDelayMilliseconds;
      this.owner.setStatus("Connected", true);
    });
    socket.addEventListener("message", (event) => {
      if (this.socket !== socket) return;
      if (typeof event.data === "string") {
        const message = parseVideoConfiguration(parseJson(event.data));
        if (!message) {
          socket.close(1003, "invalid video configuration");
          return;
        }
        if (message.version !== PROTOCOL_VERSION) {
          const error = new ProtocolVersionMismatchError(
            PROTOCOL_VERSION,
            message.version,
          );
          this.socket = null;
          this.owner.setStatus("Protocol version mismatch");
          socket.close(4002, "protocol version mismatch");
          this.owner.halt(error);
          return;
        }
        decoderSetup = this.configureDecoder(message).catch((error) => {
          if (
            this.socket !== socket ||
            this.owner.disposed() ||
            !this.owner.shouldRun()
          )
            return;
          console.error("video decoder configuration failed", error);
          this.owner.setStatus("Video decoder error");
          socket.close(4001, "video decoder configuration failed");
        });
      } else if (event.data instanceof ArrayBuffer) {
        const data = event.data;
        // A later video-config can finish before this packet's decoder setup.
        // Never submit an old profile's queued packet to the new decoder.
        const generation = this.decoderGeneration;
        void decoderSetup.then(() => {
          if (
            generation !== this.decoderGeneration ||
            this.socket !== socket ||
            this.owner.disposed() ||
            !this.owner.shouldRun()
          )
            return;
          this.decodeMessage(data);
        });
      }
    });
    socket.addEventListener("close", (event) =>
      this.handleClose(socket, event),
    );
    socket.addEventListener("error", () => socket.close());
  }

  private handleClose(socket: WebSocket, event: CloseEvent): void {
    if (this.socket !== socket) return;
    this.socket = null;
    const reason = event.reason || `WebSocket code ${event.code}`;
    console.warn("video WebSocket closed", event.code, event.reason);
    this.owner.setStatus(
      this.owner.shouldRun() ? `Reconnecting · ${event.code}` : "Disconnected",
    );
    if (this.renderedFrames === 0) {
      this.owner.updateState({ message: `${reason}. Retrying…` });
    }
    this.decoderGeneration += 1;
    this.decoder?.close();
    this.decoder = null;
    this.decodingFrames.clear();
    for (const { frame } of this.pendingFrames.splice(0)) frame.close();
    if (this.presentationTimer !== null) clearTimeout(this.presentationTimer);
    this.presentationTimer = null;
    this.decoderConfiguration = null;
    this.waitingForKeyframe = true;
    if (this.owner.shouldRun()) {
      this.reconnectTimer = setTimeout(() => {
        this.reconnectTimer = null;
        void this.connect();
      }, this.reconnectDelay);
    }
    this.reconnectDelay = Math.min(
      this.reconnectDelay * 2,
      maximumReconnectDelayMilliseconds,
    );
  }

  disconnect(): void {
    this.connectAttempt = null;
    if (this.reconnectTimer !== null) clearTimeout(this.reconnectTimer);
    this.reconnectTimer = null;
    this.socket?.close(1000, "client disconnect");
    this.socket = null;
    this.decoderGeneration += 1;
    this.decoder?.close();
    this.decoder = null;
    this.resetFeedbackInterval();
    this.decodingFrames.clear();
    for (const { frame } of this.pendingFrames.splice(0)) frame.close();
    this.cancelPresentation();
  }

  setLatencyTarget(milliseconds: number): void {
    milliseconds = Number(milliseconds);
    if (!Number.isFinite(milliseconds) || milliseconds < 0) {
      throw new RangeError("Latency must be a non-negative number");
    }
    this.targetLatencyMilliseconds = milliseconds;
    if (this.presentationTimer !== null) clearTimeout(this.presentationTimer);
    this.presentationTimer = null;
    this.schedulePresentation();
  }

  resetFeedbackInterval(now = performance.now()): void {
    this.intervalReceivedFrames = 0;
    this.intervalPresentedFrames = 0;
    this.intervalDroppedFrames = 0;
    this.queueBusyMilliseconds = 0;
    this.feedbackSampleStartedAt = now;
    this.queueObservedAt = now;
    this.queueObservedSize = this.decoder?.decodeQueueSize ?? 0;
    this.queuePeak = this.queueObservedSize;
  }

  takeFeedback(now = performance.now()): VideoFeedback | null {
    this.observeDecodeQueue(this.decoder?.decodeQueueSize ?? 0, now);
    const sampleMs = now - this.feedbackSampleStartedAt;
    const queueBusyMs = Math.min(
      Math.max(0, this.queueBusyMilliseconds),
      Math.max(0, sampleMs),
    );
    while ((this.renderedFrameTimes[0] ?? now) < now - 1000) {
      this.renderedFrameTimes.shift();
    }
    const feedback =
      Number.isFinite(sampleMs) && sampleMs > 0 && sampleMs <= 60_000
        ? {
            received: this.intervalReceivedFrames,
            presented: this.intervalPresentedFrames,
            queuePeak: this.queuePeak,
            queueBusyMs,
            sampleMs,
            dropped: this.intervalDroppedFrames,
          }
        : null;
    this.resetFeedbackInterval(now);
    return feedback;
  }

  private async configureDecoder(
    configuration: VideoConfiguration,
  ): Promise<void> {
    const generation = ++this.decoderGeneration;
    console.debug("video decoder configuration", {
      codec: configuration.codec,
      previousGeneration: this.currentGeneration,
      decoderQueue: this.decoder?.decodeQueueSize ?? 0,
      pendingVideoFrames: this.pendingFrames.length,
    });
    this.decoder?.close();
    this.decoder = null;
    this.decodingFrames.clear();
    this.cancelPresentation();
    for (const { frame } of this.pendingFrames.splice(0)) frame.close();
    this.decoderConfiguration = {
      codec: configuration.codec,
      optimizeForLatency: true,
    };
    this.waitingForKeyframe = true;
    this.renderedFrameTimes.length = 0;
    let support: VideoDecoderSupport;
    try {
      support = await VideoDecoder.isConfigSupported(this.decoderConfiguration);
    } catch (error) {
      if (
        generation !== this.decoderGeneration ||
        this.owner.disposed() ||
        !this.owner.shouldRun()
      )
        return;
      throw error;
    }
    if (generation !== this.decoderGeneration) return;
    if (!support.supported) {
      this.decoderConfiguration = null;
      this.owner.setStatus("Unsupported codec");
      this.owner.updateState({
        state: "error",
        message: `This browser cannot decode ${configuration.codec}.`,
        codec: configuration.codec,
      });
      return;
    }
    const decoder = new VideoDecoder({
      output: (frame) => {
        if (this.decoder !== decoder) {
          frame.close();
          return;
        }
        const entries = this.decodingFrames.get(frame.timestamp);
        const metadata = entries?.shift();
        if (entries?.length === 0) this.decodingFrames.delete(frame.timestamp);
        if (!metadata) {
          frame.close();
          return;
        }
        this.decodedFrames += 1;
        this.pendingFrames.push({ frame, metadata });
        // The playout buffer must cover its delay AND one bounded decoder batch
        // before the next animation frame. A fixed 24-frame cap evicts future-due
        // frames forever at 90/120 FPS with a 300ms target. This does not relax
        // the decoder-backlog limit or preallocate frames.
        const capacity =
          maximumVideoDecodeQueueSize +
          Math.ceil((metadata.fps * this.targetLatencyMilliseconds) / 1000);
        while (this.pendingFrames.length > capacity) {
          this.pendingFrames.shift()?.frame.close();
          this.droppedFrames += 1;
          this.intervalDroppedFrames += 1;
          this.decodedOverflowDroppedFrames += 1;
        }
        this.schedulePresentation();
      },
      error: (error) => {
        console.error("video decoder error", error);
        if (this.decoder === decoder) this.decoder = null;
        this.owner.setStatus(
          `Video decoder error · ${error.message || error.name}`,
        );
        if (this.socket?.readyState === WebSocket.OPEN) {
          this.socket.close(4001, "video decoder error");
        }
      },
    });
    decoder.addEventListener("dequeue", () => {
      if (this.decoder === decoder && !this.owner.disposed()) {
        this.observeDecodeQueue(decoder.decodeQueueSize);
      }
    });
    decoder.configure(this.decoderConfiguration);
    this.decoder = decoder;
    this.observeDecodeQueue(decoder.decodeQueueSize);
    this.owner.updateState({
      state: "connected",
      message: "Connected",
      codec: configuration.codec,
    });
  }

  private decodeMessage(buffer: ArrayBuffer): void {
    const packet = parseVideoPacket(buffer);
    if (!packet) return;
    this.receivedChunks += 1;
    this.intervalReceivedFrames += 1;
    if (packet.keyframe) this.receivedKeyframes += 1;
    const decoder = this.decoder;
    if (!decoder || decoder.state !== "configured") return;
    if (this.renderedFrames === 0) {
      this.owner.updateState({
        message: `Receiving video · ${this.receivedChunks} chunks · ${this.receivedKeyframes} keyframes`,
      });
    }
    this.owner.setLatestAppliedInput(packet.latestAppliedInput);
    const generationChanged = packet.generation !== this.currentGeneration;
    const previousGeneration = this.currentGeneration;
    if (generationChanged) this.currentGeneration = packet.generation;
    const queuedBeforeDecode = decoder.decodeQueueSize;
    this.observeDecodeQueue(queuedBeforeDecode);
    if (
      generationChanged ||
      packet.discontinuity ||
      queuedBeforeDecode >= maximumVideoDecodeQueueSize
    ) {
      console.debug("video decoder reset", {
        previousGeneration,
        generation: packet.generation,
        generationChanged,
        discontinuity: packet.discontinuity,
        queueOverflow: queuedBeforeDecode >= maximumVideoDecodeQueueSize,
        decoderQueue: queuedBeforeDecode,
        pendingVideoFrames: this.pendingFrames.length,
      });
      this.resetDecoder(decoder);
    }
    if (
      this.decoder !== decoder ||
      decoder.state !== "configured" ||
      (this.waitingForKeyframe && !packet.keyframe)
    )
      return;
    this.waitingForKeyframe = false;
    const metadata: FrameMetadata = {
      generation: packet.generation,
      width: packet.width,
      height: packet.height,
      fps: packet.fps,
    };
    const entries = this.decodingFrames.get(packet.timestamp);
    if (entries) entries.push(metadata);
    else this.decodingFrames.set(packet.timestamp, [metadata]);
    decoder.decode(
      new EncodedVideoChunk({
        type: packet.keyframe ? "key" : "delta",
        timestamp: packet.timestamp,
        data: packet.data,
      }),
    );
    this.observeDecodeQueue(decoder.decodeQueueSize);
  }

  private observeDecodeQueue(size: number, now = performance.now()): void {
    if (this.queueObservedSize >= busyDecodeQueueSize) {
      this.queueBusyMilliseconds += Math.max(0, now - this.queueObservedAt);
    }
    this.queueObservedSize = size;
    this.queueObservedAt = now;
    this.queuePeak = Math.max(this.queuePeak, size);
  }

  private resetDecoder(decoder: VideoDecoder): void {
    const resetFrames = decoder.decodeQueueSize + this.pendingFrames.length;
    this.observeDecodeQueue(decoder.decodeQueueSize);
    this.droppedFrames += resetFrames;
    this.intervalDroppedFrames += resetFrames;
    this.decoderResetDroppedFrames += resetFrames;
    this.decoderResets += 1;
    this.cancelPresentation();
    for (const { frame } of this.pendingFrames.splice(0)) frame.close();
    this.decodingFrames.clear();
    this.waitingForKeyframe = true;
    decoder.reset();
    this.observeDecodeQueue(decoder.decodeQueueSize);
    if (this.decoderConfiguration) decoder.configure(this.decoderConfiguration);
  }

  private cancelPresentation(): void {
    if (this.presentationTimer !== null) clearTimeout(this.presentationTimer);
    if (this.animationFrame !== null) cancelAnimationFrame(this.animationFrame);
    this.presentationTimer = null;
    this.animationFrame = null;
    this.animationPending = false;
  }

  private schedulePresentation(): void {
    if (
      this.pendingFrames.length === 0 ||
      this.animationPending ||
      this.presentationTimer !== null
    )
      return;
    const next = this.pendingFrames[0];
    if (!next) return;
    const presentation = this.owner.expectedPresentationTime(
      next.frame.timestamp,
      this.targetLatencyMilliseconds,
    );
    const delay = presentation === null ? 0 : presentation - performance.now();
    if (delay > presentationLeadMilliseconds) {
      this.presentationTimer = setTimeout(
        () => {
          this.presentationTimer = null;
          this.schedulePresentation();
        },
        Math.min(
          delay - presentationWakeMarginMilliseconds,
          maximumPresentationTimerMilliseconds,
        ),
      );
      return;
    }
    this.animationPending = true;
    this.animationFrame = requestAnimationFrame(() => this.renderFrame());
  }

  private renderFrame(): void {
    this.animationFrame = null;
    this.animationPending = false;
    if (this.pendingFrames.length === 0) return;
    const display = this.display;
    const context = this.context;
    if (!display || !context) {
      for (const { frame } of this.pendingFrames.splice(0)) frame.close();
      return;
    }
    const now = performance.now();
    let pending: PendingVideoFrame | undefined;
    const synchronized = this.owner.statsContext().clockConfident;
    if (!synchronized) {
      pending = this.pendingFrames.pop();
      for (const { frame } of this.pendingFrames.splice(0)) {
        frame.close();
        this.recordOverdueDrop();
      }
    } else {
      let due = 0;
      while (due < this.pendingFrames.length) {
        const candidate = this.pendingFrames[due];
        if (!candidate) break;
        const presentation = this.owner.expectedPresentationTime(
          candidate.frame.timestamp,
          this.targetLatencyMilliseconds,
        );
        if (presentation !== null && presentation > now) break;
        due += 1;
      }
      if (due === 0) {
        this.schedulePresentation();
        return;
      }
      const ready = this.pendingFrames.splice(0, due);
      pending = ready.pop();
      for (const { frame } of ready) {
        frame.close();
        this.recordOverdueDrop();
      }
    }
    if (!pending) return;
    const { frame, metadata } = pending;
    const dimensionsChanged =
      display.width !== frame.displayWidth ||
      display.height !== frame.displayHeight;
    if (dimensionsChanged) {
      display.width = frame.displayWidth;
      display.height = frame.displayHeight;
    }
    context.drawImage(frame, 0, 0, display.width, display.height);
    const drawCompletedAtMs = performance.now();
    // Presented means rendered to the canvas, not physical display scanout.
    this.owner.presentResizeGeneration(
      metadata.generation,
      metadata.width,
      metadata.height,
    );
    this.renderedFrames += 1;
    if (this.renderedFrames === 1) {
      this.owner.updateState({ message: "Streaming video" });
    }
    const presentation = this.owner.expectedPresentationTime(
      frame.timestamp,
      this.targetLatencyMilliseconds,
    );
    this.videoLateness = presentation === null ? 0 : now - presentation;
    const renderedMediaTimestampMicros = frame.timestamp;
    frame.close();
    this.presentedFrames += 1;
    this.intervalPresentedFrames += 1;
    this.renderedFrameTimes.push(now);
    this.lastDrawStats = Object.freeze({
      width: display.width,
      height: display.height,
      renderedMediaTimestampMicros,
      drawCompletedAtMs,
      generation: metadata.generation,
      latenessMs: this.videoLateness,
      ...this.diagnostics(now),
    });
    if (now - this.lastStatsPublishedAt >= this.statsIntervalMilliseconds) {
      this.lastStatsPublishedAt = now;
      this.owner.publishStats(this.lastDrawStats);
    }
    this.schedulePresentation();
  }

  private recordOverdueDrop(): void {
    this.droppedFrames += 1;
    // Newest-due selection is normal when source FPS exceeds display refresh.
    // Retain the diagnostic count, but do not report it as capacity loss.
    this.overdueDroppedFrames += 1;
  }
}
