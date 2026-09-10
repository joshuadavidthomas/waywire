import { LocalCursor } from "./cursor.ts";
import { InputRuntime, SurfaceListeners } from "./input.ts";
import { parseControlMessage, parseJson } from "./messages.ts";
import {
  automaticResizeAlignment,
  fitObservedResize,
  normalizeResizeDimensions,
  type RemoteDisplayPolicy,
} from "./resize.ts";
import { VideoRuntime } from "./video.ts";
import { resize as resizeRecord } from "./wire.ts";
import type {
  SurfaceHandle,
  SurfaceOptions,
  VideoState,
  WaymoteEventMap,
  WaymoteSessionOptions,
  WaymoteSessionState,
  WaymoteStats,
} from "./session.ts";

export type TransportPath = "/stream" | "/control";
export type WebSocketFactory = (
  path: TransportPath,
  url: URL,
) => WebSocket | Promise<WebSocket>;

const clockMaximumAgeMilliseconds = 5000;
const clockResetAgeMilliseconds = 30_000;
const maximumClockSampleRttMilliseconds = 60_000;

export class SessionTransport {
  constructor(
    private readonly endpoint: string | URL | undefined,
    private readonly factory: WebSocketFactory | undefined,
  ) {}

  private websocketURL(path: TransportPath): URL {
    const url = new URL(path, this.endpoint ?? location.origin);
    url.protocol = url.protocol === "https:" ? "wss:" : "ws:";
    return url;
  }

  async create(path: TransportPath): Promise<WebSocket> {
    const url = this.websocketURL(path);
    const socket = this.factory
      ? await this.factory(path, url)
      : new WebSocket(url);
    if (
      !socket ||
      typeof socket.addEventListener !== "function" ||
      typeof socket.close !== "function"
    ) {
      throw new TypeError(
        "createWebSocket must return a WebSocket-compatible object",
      );
    }
    return socket;
  }
}

export class ClockSynchronizer {
  offsetMicros: number | null = null;
  bestRttMilliseconds = Number.POSITIVE_INFINITY;
  private sampleCount = 0;
  private lastSampleAt = 0;

  synchronized(now = performance.now()): boolean {
    return (
      this.offsetMicros !== null &&
      this.sampleCount >= 2 &&
      now - this.lastSampleAt <= clockMaximumAgeMilliseconds
    );
  }

  update(sent: number, received: number, serverNanos: string | number): void {
    const serverMicros = Number(serverNanos) / 1000;
    const sampleRtt = received - sent;
    if (
      !Number.isFinite(serverMicros) ||
      serverMicros < 0 ||
      !Number.isFinite(sampleRtt) ||
      sampleRtt < 0 ||
      sampleRtt > maximumClockSampleRttMilliseconds
    ) {
      return;
    }
    const candidateOffset = (sent + received) * 500 - serverMicros;
    if (
      this.offsetMicros === null ||
      received - this.lastSampleAt > clockResetAgeMilliseconds
    ) {
      this.offsetMicros = candidateOffset;
      this.bestRttMilliseconds = sampleRtt;
      this.sampleCount = 1;
      this.lastSampleAt = received;
    } else if (
      sampleRtt <=
      this.bestRttMilliseconds + Math.max(2, this.bestRttMilliseconds * 0.25)
    ) {
      const correction = candidateOffset - this.offsetMicros;
      this.offsetMicros +=
        Math.abs(correction) > 100_000 ? correction : correction * 0.1;
      this.bestRttMilliseconds = Math.min(this.bestRttMilliseconds, sampleRtt);
      this.sampleCount += 1;
      this.lastSampleAt = received;
    }
  }

  expectedPresentationTime(
    captureMicros: number,
    targetLatencyMilliseconds: number,
  ): number | null {
    if (!this.synchronized() || !Number.isFinite(captureMicros)) return null;
    return this.offsetMicros === null
      ? null
      : (captureMicros + this.offsetMicros) / 1000 + targetLatencyMilliseconds;
  }
}

type Scale = Readonly<{ x: number; y: number }>;
type ConnectionAttempt = Readonly<{ generation: number }>;
type PointerPosition = Readonly<{ x: number; y: number }>;
type ContentBounds = Readonly<{
  width: number;
  height: number;
  left: number;
  top: number;
}>;
type ResizeRequest = { requested: number; generation: number };
type PendingClipboardCopy = {
  resolve: (text: string) => void;
  reject: (cause: unknown) => void;
  timeout: ReturnType<typeof setTimeout>;
};
type ShortcutClipboardCopy = {
  available: boolean;
  text: string;
  timeout: ReturnType<typeof setTimeout>;
};
type RuntimeOwner = {
  emit<K extends keyof WaymoteEventMap>(
    type: K,
    value: WaymoteEventMap[K],
  ): void;
  updateState<K extends keyof WaymoteSessionState>(
    section: K,
    changes: Partial<WaymoteSessionState[K]>,
  ): void;
  setStats(stats: WaymoteStats): void;
  halt(error: Error): void;
  remoteDisplayPolicy(): RemoteDisplayPolicy;
  controlOnFocus(): boolean;
  setControlOnFocus(enabled: boolean): void;
  setRemoteDisplayPolicy(policy: RemoteDisplayPolicy): void;
};
function toError(cause: unknown): Error {
  return cause instanceof Error ? cause : new Error(String(cause));
}

type Timer = ReturnType<typeof setTimeout>;

export class ControlRuntime {
  private readonly scheduleResizeBound = () => this.scheduleResize();

  private readonly owner: RuntimeOwner;
  private readonly options: WaymoteSessionOptions;
  private readonly transport: SessionTransport;
  private readonly video: VideoRuntime;

  constructor(owner: RuntimeOwner, options: WaymoteSessionOptions) {
    this.owner = owner;
    this.options = options;
    this.transport = new SessionTransport(
      options.endpoint,
      options.createWebSocket,
    );
    this.video = this.createVideo();
  }

  private display: HTMLCanvasElement | null = null;
  private inputElement: HTMLElement | null = null;
  private localCursor: LocalCursor | null = null;
  private imeProxy: HTMLInputElement | HTMLTextAreaElement | null = null;
  private context: CanvasRenderingContext2D | null = null;
  private readonly surfaceListeners = new SurfaceListeners();
  private resizeObserver: ResizeObserver | null = null;
  private feedbackTimer: Timer | null = null;
  private clipboardAutoSync = false;
  private sessionConnected = false;
  private sessionDisposed = false;
  private connectionGeneration = 0;
  private surfaceDisposer: (() => void) | null = null;
  private disposePromise: Promise<void> | null = null;
  private readonly clock = new ClockSynchronizer();

  private readonly normalizedPointerExtent = 65_535;
  private readonly maximumClipboardBytes = 1024 * 1024;
  private readonly clipboardCopyTimeoutMilliseconds = 2000;

  private controlSocket: WebSocket | null = null;
  private controlReconnectDelay = 250;
  private controlReconnectTimer: Timer | null = null;
  private controlStableTimer: Timer | null = null;
  private controlConnectAttempt: ConnectionAttempt | null = null;
  private controlAcquireDelay = 250;
  private controlAcquireTimer: Timer | null = null;
  private controlActive = false;
  private controlWanted = false;
  private pendingControlRecords: ArrayBuffer[] = [];
  private resizeTimer: Timer | null = null;
  private resizePending = false;
  private lastResizeRequest: string | null = null;
  private resizeRequestID = 0;
  private qualityBitrate = 0;
  private qualityScale = 100;
  private remoteClipboard: string | null = null;
  private pendingClipboardCopy: PendingClipboardCopy | null = null;
  private shortcutClipboardCopy: ShortcutClipboardCopy | null = null;
  private inputSequence = 0;
  private latestAppliedInput = 0;
  private rtt = 0;
  private pingID = 0;
  private readonly pings = new Map<number, number>();
  private readonly resizeRequests = new Map<number, ResizeRequest>();
  private resizeState: WaymoteStats["resizeState"] = "idle";
  private createVideo(): VideoRuntime {
    return new VideoRuntime(
      {
        createSocket: () => this.transport.create("/stream"),
        connectionGeneration: () => this.connectionGeneration,
        shouldRun: () => this.sessionConnected && !document.hidden,
        disposed: () => this.sessionDisposed,
        setStatus: (text, connected) => this.setStatus(text, connected),
        updateState: (changes) => this.owner.updateState("video", changes),
        emitError: (error) => this.emit("error", error),
        halt: (error) => this.owner.halt(error),
        expectedPresentationTime: (captureMicros, latencyMilliseconds) =>
          this.clock.expectedPresentationTime(
            captureMicros,
            latencyMilliseconds,
          ),
        statsContext: () => ({
          bitrateKbps: this.qualityBitrate,
          scalePercent: this.qualityScale,
          rttMs: this.rtt,
          clockConfident: this.clock.synchronized(),
          clockUncertaintyMs: Number.isFinite(this.clock.bestRttMilliseconds)
            ? this.clock.bestRttMilliseconds / 2
            : null,
          pendingInputCount: Math.max(
            0,
            this.inputSequence - this.latestAppliedInput,
          ),
          resizeState: this.resizeState,
        }),
        setLatestAppliedInput: (sequence) => {
          this.latestAppliedInput = sequence;
        },
        presentResizeGeneration: (generation, width, height) =>
          this.presentResizeGeneration(generation, width, height),
        publishStats: (stats) => this.owner.setStats(stats),
      },
      this.options.latency,
      this.options.statsIntervalMs,
    );
  }

  private readonly inputRuntime = new InputRuntime({
    display: () => this.display,
    inputElement: () => this.inputElement,
    imeProxy: () => this.imeProxy,
    contentPosition: (event) => this.contentPosition(event),
    sendRecord: (record) => this.sendControl(record),
    nextSequence: () => this.nextInputSequence(),
    requestControl: () => this.requestControl(),
    releaseControl: () => this.releaseControl(),
    controlActive: () => this.controlActive,
    controlOnFocus: () => this.owner.controlOnFocus(),
    sendLocalClipboard: () => this.sendLocalClipboard(),
    disposed: () => this.sessionDisposed,
    setClipboardUnavailable: (error) => {
      console.warn("local clipboard sync failed", error);
      this.setClipboardStatus("Local clipboard unavailable");
    },
    armRemoteCopy: () => {
      this.armShortcutClipboardCopy();
      this.reserveRemoteClipboardCopy();
    },
    completeRemoteCopy: () => this.completeShortcutClipboardCopy(),
    sendText: (action, text) => {
      if (this.controlActive) {
        this.controlSocket?.send(
          JSON.stringify({
            type: "text",
            action,
            text,
            sequence: this.nextInputSequence(),
          }),
        );
      }
    },
    updatePointerLocked: (pointerLocked) =>
      this.owner.updateState("input", { pointerLocked }),
    visibilityChanged: () => this.handleVisibilityChange(),
    emitError: (error) => this.emit("error", error),
  });
  private emit<K extends keyof WaymoteEventMap>(
    type: K,
    value: WaymoteEventMap[K],
  ): void {
    this.owner.emit(type, value);
  }

  private readonly createWebSocket = (
    path: TransportPath,
  ): Promise<WebSocket> => this.transport.create(path);

  private setStatus(text: string, connected = false): void {
    const state = connected
      ? "connected"
      : text.startsWith("Reconnecting")
        ? "reconnecting"
        : text.includes("connecting") || text === "Connecting"
          ? "connecting"
          : text.includes("error") || text.includes("unavailable")
            ? "error"
            : "disconnected";
    this.owner.updateState("video", { state, message: text });
  }

  private setControlStatus(text: string, connected = false): void {
    const state =
      text === "Input active"
        ? "active"
        : text.includes("requesting")
          ? "requesting"
          : text.includes("use")
            ? "busy"
            : text.includes("ready")
              ? "ready"
              : text.includes("connecting") || text.includes("reconnecting")
                ? "connecting"
                : "disconnected";
    this.owner.updateState("input", { state, message: text, connected });
  }

  private setClipboardStatus(text: string): void {
    this.emit(
      "clipboard",
      Object.freeze({ text: this.remoteClipboard, status: text }),
    );
  }

  private readonly updateClock = (
    sent: number,
    received: number,
    serverNanos: string | number,
  ): void => this.clock.update(sent, received, serverNanos);
  public getRemoteClipboard(): string | null {
    return this.remoteClipboard;
  }

  public sendClipboardText(text: string): boolean {
    if (
      new TextEncoder().encode(text).byteLength > this.maximumClipboardBytes
    ) {
      this.setClipboardStatus("Clipboard is too large");
      return false;
    }
    if (
      !this.controlActive ||
      !this.controlSocket ||
      this.controlSocket.readyState !== WebSocket.OPEN
    ) {
      this.setClipboardStatus("Clipboard needs input control");
      return false;
    }
    this.controlSocket.send(JSON.stringify({ type: "clipboard-write", text }));
    this.setClipboardStatus("Local clipboard sent");
    return true;
  }

  private async sendLocalClipboard(): Promise<boolean> {
    if (!navigator.clipboard?.readText) {
      throw new Error("Clipboard read is unavailable");
    }
    const text = await navigator.clipboard.readText();
    if (this.sessionDisposed) return false;
    if (!this.sendClipboardText(text)) {
      throw new Error("Clipboard control is inactive");
    }
    return true;
  }

  private copyTextFallback(text: string): boolean {
    let handled = false;
    const handleCopy = (event: ClipboardEvent) => {
      if (!event.clipboardData) {
        return;
      }
      event.clipboardData.setData("text/plain", text);
      event.preventDefault();
      handled = true;
    };
    document.addEventListener("copy", handleCopy);
    const copied = document.execCommand("copy");
    document.removeEventListener("copy", handleCopy);
    return copied && handled;
  }

  private reserveRemoteClipboardCopy(): boolean {
    if (
      !navigator.clipboard?.write ||
      typeof ClipboardItem === "undefined" ||
      this.pendingClipboardCopy
    ) {
      return false;
    }

    let resolveText: (text: string) => void = () => undefined;
    let rejectText: (cause: unknown) => void = () => undefined;
    const text = new Promise<string>((resolve, reject) => {
      resolveText = resolve;
      rejectText = reject;
    });
    const transaction: PendingClipboardCopy = {
      resolve: resolveText,
      reject: rejectText,
      timeout: setTimeout(() => {
        if (this.pendingClipboardCopy !== transaction) return;
        this.pendingClipboardCopy = null;
        transaction.reject(new Error("Remote copy timed out"));
      }, this.clipboardCopyTimeoutMilliseconds),
    };
    this.pendingClipboardCopy = transaction;

    let write;
    try {
      write = navigator.clipboard.write([
        new ClipboardItem({
          "text/plain": text.then(
            (value) => new Blob([value], { type: "text/plain" }),
          ),
        }),
      ]);
    } catch (error) {
      clearTimeout(transaction.timeout);
      this.pendingClipboardCopy = null;
      transaction.reject(error);
      return false;
    }
    write.then(
      () => {
        if (!this.sessionDisposed)
          this.setClipboardStatus("Remote clipboard copied");
      },
      (error) => {
        if (this.sessionDisposed) return;
        if (this.pendingClipboardCopy === transaction) {
          clearTimeout(transaction.timeout);
          this.pendingClipboardCopy = null;
          transaction.reject(error);
        }
        console.warn("remote shortcut copy failed", error);
        this.setClipboardStatus("Remote clipboard ready");
      },
    );
    return true;
  }

  private armShortcutClipboardCopy(): void {
    if (this.shortcutClipboardCopy) {
      clearTimeout(this.shortcutClipboardCopy.timeout);
    }
    const transaction: ShortcutClipboardCopy = {
      available: false,
      text: "",
      timeout: setTimeout(() => {
        if (this.shortcutClipboardCopy === transaction)
          this.shortcutClipboardCopy = null;
      }, this.clipboardCopyTimeoutMilliseconds),
    };
    this.shortcutClipboardCopy = transaction;
  }

  private completeShortcutClipboardCopy(): void {
    const transaction = this.shortcutClipboardCopy;
    if (!transaction?.available || !this.copyTextFallback(transaction.text)) {
      return;
    }
    clearTimeout(transaction.timeout);
    this.shortcutClipboardCopy = null;
    this.setClipboardStatus("Remote clipboard copied");
  }

  private receiveRemoteClipboard(text: string): void {
    this.remoteClipboard = text;
    this.setClipboardStatus(
      text.length === 0 ? "Remote clipboard empty" : "Remote clipboard ready",
    );
    if (this.shortcutClipboardCopy) {
      this.shortcutClipboardCopy.text = text;
      this.shortcutClipboardCopy.available = true;
    }
    if (this.pendingClipboardCopy) {
      const transaction = this.pendingClipboardCopy;
      this.pendingClipboardCopy = null;
      clearTimeout(transaction.timeout);
      transaction.resolve(text);
      return;
    }
    if (text.length === 0) {
      return;
    }
    if (
      this.clipboardAutoSync &&
      !document.hidden &&
      document.hasFocus() &&
      navigator.clipboard?.writeText
    ) {
      navigator.clipboard.writeText(text).then(
        () => this.setClipboardStatus("Clipboard synced"),
        () => this.setClipboardStatus("Remote clipboard ready"),
      );
    }
  }

  private updateControlStatus(): void {
    if (
      !this.controlSocket ||
      this.controlSocket.readyState !== WebSocket.OPEN
    ) {
      return;
    }
    if (this.controlActive) {
      this.setControlStatus("Input active", true);
    } else if (this.controlWanted) {
      this.setControlStatus("Input requesting");
    } else {
      this.setControlStatus("Input ready · click stream", true);
    }
  }

  private nextInputSequence(): number {
    this.inputSequence = (this.inputSequence + 1) >>> 0;
    if (this.inputSequence === 0) {
      this.inputSequence = 1;
    }
    return this.inputSequence;
  }

  private visibleContentBounds(): ContentBounds {
    const currentDisplay = this.display;
    if (!currentDisplay) throw new Error("No this.display is attached");
    const bounds = currentDisplay.getBoundingClientRect();
    const contentAspect =
      this.video.renderedFrameCount > 0 &&
      currentDisplay.width > 0 &&
      currentDisplay.height > 0
        ? currentDisplay.width / currentDisplay.height
        : 16 / 9;
    let width = bounds.width;
    let height = bounds.height;
    let left = bounds.left;
    let top = bounds.top;
    if (width / height > contentAspect) {
      width = height * contentAspect;
      left += (bounds.width - width) / 2;
    } else {
      height = width / contentAspect;
      top += (bounds.height - height) / 2;
    }
    return { width, height, left, top };
  }

  private presentResizeGeneration(
    generation: number,
    width: number,
    height: number,
  ): WaymoteStats["resizeState"] {
    for (const [id, request] of this.resizeRequests) {
      if (request.generation !== generation) continue;
      this.resizeState = "presented";
      this.emit(
        "resize",
        Object.freeze({
          state: this.resizeState,
          latencyMs: performance.now() - request.requested,
          width,
          height,
          generation,
        }),
      );
      this.resizeRequests.delete(id);
    }
    return this.resizeState;
  }

  private sendResize(): void {
    if (!this.controlActive) {
      return;
    }
    const policy = this.owner.remoteDisplayPolicy();
    if (policy.mode === "manual") return;
    if (policy.mode === "fixed") {
      this.sendResizeDimensions(
        policy.width,
        policy.height,
        policy.scale * 120,
      );
      return;
    }
    const observed = policy.element ?? this.display;
    if (!observed) return;
    const bounds = observed.getBoundingClientRect();
    if (bounds.width <= 0 || bounds.height <= 0) return;
    const dpr = policy.devicePixelRatio ?? window.devicePixelRatio ?? 1;
    const rawWidth = bounds.width * dpr;
    const rawHeight = bounds.height * dpr;
    const { width, height, downscale } = fitObservedResize(
      rawWidth,
      rawHeight,
      policy,
    );
    const scale = Math.max(
      120,
      Math.min(480, Math.round(dpr * downscale * 120)),
    );
    const previous = this.lastResizeRequest?.match(/^(\d+)x(\d+)@(\d+)$/);
    if (
      previous &&
      Number(previous[3]) === scale &&
      Math.abs(Number(previous[1]) - width) <= automaticResizeAlignment * 2 &&
      Math.abs(Number(previous[2]) - height) <= automaticResizeAlignment * 2
    ) {
      return;
    }
    this.sendResizeDimensions(width, height, scale);
  }

  private sendResizeDimensions(
    width: number,
    height: number,
    scale: number,
  ): void {
    ({ width, height } = normalizeResizeDimensions(width, height));
    scale = Math.max(120, Math.min(480, Math.round(scale)));
    const request = `${width}x${height}@${scale}`;
    if (request === this.lastResizeRequest) {
      return;
    }
    this.resizeRequestID = (this.resizeRequestID + 1) & 0xffff;
    if (this.resizeRequestID === 0) {
      this.resizeRequestID = 1;
    }
    const record = resizeRecord(width, height, scale, this.resizeRequestID);
    this.resizeRequests.set(this.resizeRequestID, {
      requested: performance.now(),
      generation: 0,
    });
    this.resizeState = "requested";
    this.emit(
      "resize",
      Object.freeze({
        state: this.resizeState,
        width,
        height,
        scale: scale / 120,
      }),
    );
    if (this.sendControl(record)) {
      this.lastResizeRequest = request;
    }
  }

  private flushResize(): void {
    this.resizeTimer = null;
    if (!this.resizePending) {
      return;
    }
    this.resizePending = false;
    this.sendResize();
  }

  private scheduleResize(): void {
    const policy = this.owner.remoteDisplayPolicy();
    if (policy.mode !== "observe") return;
    if (this.resizeTimer === null) {
      this.sendResize();
      this.resizeTimer = setTimeout(
        () => this.flushResize(),
        policy.debounceMs ?? 100,
      );
      return;
    }
    this.resizePending = true;
  }

  private sendControl(record: ArrayBuffer): boolean {
    if (!this.controlSocket) {
      return false;
    }
    if (this.controlSocket.readyState === WebSocket.CONNECTING) {
      if (this.pendingControlRecords.length >= 64) {
        this.pendingControlRecords = [];
        this.controlSocket.close(4001, "input queue full");
        this.setControlStatus("Input overloaded");
        return false;
      }
      this.pendingControlRecords.push(record);
      return true;
    }
    if (this.controlSocket.readyState !== WebSocket.OPEN) {
      return false;
    }
    this.controlSocket.send(record);
    return true;
  }

  private releaseInput(): void {
    this.inputRuntime.release();
  }

  public requestControl(): void {
    this.controlWanted = true;
    if (this.controlAcquireTimer !== null) {
      clearTimeout(this.controlAcquireTimer);
      this.controlAcquireTimer = null;
    }
    if (this.sessionConnected) void this.connectControl();
    if (
      this.controlSocket &&
      this.controlSocket.readyState === WebSocket.OPEN
    ) {
      this.controlSocket.send("acquire");
      this.setControlStatus("Input requesting");
    }
  }

  public releaseControl(): void {
    this.localCursor?.clear();
    this.controlWanted = false;
    this.controlActive = false;
    this.lastResizeRequest = null;
    if (this.controlAcquireTimer !== null) {
      clearTimeout(this.controlAcquireTimer);
      this.controlAcquireTimer = null;
    }
    this.releaseInput();
    this.pendingControlRecords = [];
    if (
      this.controlSocket &&
      this.controlSocket.readyState === WebSocket.OPEN
    ) {
      this.controlSocket.send("release");
      if (this.sessionConnected)
        this.setControlStatus("Input ready · click stream", true);
    } else if (this.sessionConnected) {
      this.setControlStatus("Input connecting");
    }
  }

  private retryControlAcquire(): void {
    if (
      !this.controlWanted ||
      !this.controlSocket ||
      this.controlSocket.readyState !== WebSocket.OPEN
    ) {
      return;
    }
    this.controlSocket.send("acquire");
    this.controlAcquireDelay = Math.min(this.controlAcquireDelay * 2, 2000);
    this.controlAcquireTimer = setTimeout(
      () => this.retryControlAcquire(),
      this.controlAcquireDelay,
    );
  }

  private async connectControl(): Promise<void> {
    if (!this.sessionConnected || document.hidden) return;
    if (this.controlReconnectTimer !== null) {
      clearTimeout(this.controlReconnectTimer);
      this.controlReconnectTimer = null;
    }
    if (
      this.controlSocket &&
      (this.controlSocket.readyState === WebSocket.CONNECTING ||
        this.controlSocket.readyState === WebSocket.OPEN)
    ) {
      return;
    }
    if (this.controlConnectAttempt) return;
    this.setControlStatus("Input connecting");
    const attempt = { generation: this.connectionGeneration };
    this.controlConnectAttempt = attempt;
    let socket;
    try {
      socket = await this.createWebSocket("/control");
    } catch (error) {
      if (this.controlConnectAttempt !== attempt) return;
      this.controlConnectAttempt = null;
      if (
        !this.sessionConnected ||
        document.hidden ||
        attempt.generation !== this.connectionGeneration
      )
        return;
      const failure = error instanceof Error ? error : new Error(String(error));
      this.emit("error", failure);
      this.setControlStatus(`Input reconnecting · ${failure.message}`);
      this.controlReconnectTimer = setTimeout(() => {
        this.controlReconnectTimer = null;
        void this.connectControl();
      }, this.controlReconnectDelay);
      this.controlReconnectDelay = Math.min(
        this.controlReconnectDelay * 2,
        5000,
      );
      return;
    }
    if (
      this.controlConnectAttempt !== attempt ||
      !this.sessionConnected ||
      document.hidden ||
      attempt.generation !== this.connectionGeneration
    ) {
      if (this.controlConnectAttempt === attempt)
        this.controlConnectAttempt = null;
      socket.close(1000, "stale connection attempt");
      return;
    }
    this.controlConnectAttempt = null;
    socket.binaryType = "arraybuffer";
    this.controlSocket = socket;
    socket.addEventListener("open", () => {
      if (this.controlSocket !== socket) {
        socket.close();
        return;
      }
      this.video.resetFeedbackInterval();
      if (this.controlWanted) {
        socket.send("acquire");
      }
      for (const record of this.pendingControlRecords) {
        this.sendControl(record);
      }
      this.pendingControlRecords = [];
      this.updateControlStatus();
      if (this.controlStableTimer !== null)
        clearTimeout(this.controlStableTimer);
      this.controlStableTimer = setTimeout(() => {
        this.controlStableTimer = null;
        if (
          this.controlSocket === socket &&
          socket.readyState === WebSocket.OPEN
        ) {
          this.controlReconnectDelay = 250;
        }
      }, 5000);
    });
    socket.addEventListener("message", (event) => {
      if (this.controlSocket !== socket || typeof event.data !== "string") {
        return;
      }
      const message = parseControlMessage(parseJson(event.data));
      if (!message) {
        socket.close(1003, "invalid control state");
        return;
      }
      if (message.type !== "control-state") {
        if (message.type === "pong") {
          const sent = this.pings.get(message.id);
          if (sent !== undefined) {
            const received = performance.now();
            this.rtt = received - sent;
            this.pings.delete(message.id);
            this.updateClock(sent, received, message.serverNanos);
          }
        }
        if (message.type === "resize-applied") {
          const request = this.resizeRequests.get(message.request);
          if (request) {
            request.generation = message.generation;
            this.resizeState = "applied";
            this.emit(
              "resize",
              Object.freeze({
                state: this.resizeState,
                latencyMs: performance.now() - request.requested,
                width: message.width,
                height: message.height,
                scale: message.scale / 120,
                generation: message.generation,
              }),
            );
          }
        }
        if (message.type === "cursor") {
          try {
            this.localCursor?.update(message);
          } catch {
            socket.close(1003, "invalid cursor state");
          }
        }
        if (message.type === "clipboard")
          this.receiveRemoteClipboard(message.text);
        if (message.type === "quality") {
          this.qualityBitrate = message.bitrate;
          this.qualityScale = message.scale;
          this.emit(
            "quality",
            Object.freeze({
              bitrateKbps: this.qualityBitrate,
              frameRate: message.fps,
              scalePercent: this.qualityScale,
            }),
          );
        }
        return;
      }
      const wasControlActive = this.controlActive;
      this.controlActive = message.state === "active";
      this.localCursor?.setActive(this.controlActive && this.controlWanted);
      if (this.controlActive) {
        this.controlAcquireDelay = 250;
        if (this.controlAcquireTimer !== null) {
          clearTimeout(this.controlAcquireTimer);
          this.controlAcquireTimer = null;
        }
        if (!this.controlWanted) {
          this.controlActive = false;
          socket.send("release");
        } else if (!wasControlActive) {
          this.lastResizeRequest = null;
          this.sendResize();
        }
      } else if (message.state === "busy" && this.controlWanted) {
        this.lastResizeRequest = null;
        this.inputRuntime.resetPressed();
        this.setControlStatus("Input in use");
        if (this.controlAcquireTimer === null) {
          this.controlAcquireTimer = setTimeout(
            () => this.retryControlAcquire(),
            this.controlAcquireDelay,
          );
        }
        return;
      } else if (message.state === "ready" && this.controlWanted) {
        this.lastResizeRequest = null;
        this.requestControl();
        return;
      }
      this.updateControlStatus();
    });
    socket.addEventListener("close", (event) => {
      if (this.controlSocket !== socket) {
        return;
      }
      this.controlSocket = null;
      this.localCursor?.clear();
      this.controlActive = false;
      this.lastResizeRequest = null;
      this.pendingControlRecords = [];
      this.inputRuntime.resetPressed();
      if (this.controlAcquireTimer !== null) {
        clearTimeout(this.controlAcquireTimer);
        this.controlAcquireTimer = null;
      }
      if (this.sessionConnected && !document.hidden) {
        this.setControlStatus(`Input reconnecting · ${event.code}`);
        this.controlReconnectTimer = setTimeout(() => {
          this.controlReconnectTimer = null;
          void this.connectControl();
        }, this.controlReconnectDelay);
        this.controlReconnectDelay = Math.min(
          this.controlReconnectDelay * 2,
          5000,
        );
      } else {
        this.setControlStatus("Input disconnected");
      }
    });
    socket.addEventListener("error", () => socket.close());
  }

  private contentPosition(event: PointerEvent): PointerPosition | null {
    const currentDisplay = this.display;
    if (
      !currentDisplay ||
      currentDisplay.width === 0 ||
      currentDisplay.height === 0
    ) {
      return null;
    }
    const { width, height, left, top } = this.visibleContentBounds();
    const x = (event.clientX - left) / width;
    const y = (event.clientY - top) / height;
    if (x < 0 || x > 1 || y < 0 || y > 1) {
      return null;
    }
    return {
      x: Math.round(x * this.normalizedPointerExtent),
      y: Math.round(y * this.normalizedPointerExtent),
    };
  }

  private handleVisibilityChange(): void {
    if (document.hidden) {
      this.releaseControl();
      this.controlConnectAttempt = null;
      this.video.closeForHiddenPage();
    } else if (this.sessionConnected) {
      void this.connectControl();
      if (!this.video.hasSocket) void this.video.connect();
      if (
        this.owner.controlOnFocus() &&
        document.activeElement === this.inputElement
      ) {
        this.requestControl();
      }
    }
  }

  private sendFeedback(): void {
    const now = performance.now();
    const feedback = this.video.takeFeedback(now);
    const socket = this.controlSocket;
    if (socket && socket.readyState === WebSocket.OPEN) {
      const id = ++this.pingID;
      this.pings.set(id, now);
      for (const [pendingID, sent] of this.pings) {
        if (sent < now - 10_000) this.pings.delete(pendingID);
      }
      socket.send(JSON.stringify({ type: "ping", id }));
      if (feedback)
        socket.send(
          JSON.stringify({ type: "feedback", ...feedback, rtt: this.rtt }),
        );
    }
  }

  private refreshResizeObservation(): void {
    this.resizeObserver?.disconnect();
    this.resizeObserver = null;
    if (typeof window === "undefined") return;
    window.removeEventListener("resize", this.scheduleResizeBound);

    const policy = this.owner.remoteDisplayPolicy();
    if (!this.sessionConnected || !this.display) return;
    const observedDisplay =
      policy.mode === "observe"
        ? (policy.element ?? this.display)
        : this.display;
    if (typeof ResizeObserver !== "undefined") {
      this.resizeObserver = new ResizeObserver(() => this.scheduleResize());
      this.resizeObserver.observe(this.inputElement ?? this.display);
      if (observedDisplay !== this.inputElement)
        this.resizeObserver.observe(observedDisplay);
    }
    if (policy.mode === "observe") {
      window.addEventListener("resize", this.scheduleResizeBound);
    }
    this.scheduleResize();
  }

  public remoteDisplayPolicyChanged(): void {
    this.lastResizeRequest = null;
    if (this.resizeTimer !== null) {
      clearTimeout(this.resizeTimer);
      this.resizeTimer = null;
    }
    this.resizePending = false;
    this.refreshResizeObservation();
    this.sendResize();
  }

  private addSurfaceListener<E extends Event>(
    target: EventTarget,
    type: string,
    listener: (event: E) => void,
    options?: AddEventListenerOptions | boolean,
  ): void {
    this.surfaceListeners.add(target, type, listener, options);
  }

  public attachSurface(surfaceOptions: SurfaceOptions): SurfaceHandle {
    if (this.sessionDisposed) {
      throw new Error("The session has been disposed");
    }
    if (!surfaceOptions?.canvas) {
      throw new TypeError("attachSurface requires a canvas");
    }
    if (this.display) {
      throw new Error("A surface is already attached to this session");
    }

    const attachedCanvas = surfaceOptions.canvas;
    const attachedInput = surfaceOptions.inputElement ?? attachedCanvas;
    const createdImeProxy = surfaceOptions.textInputElement
      ? null
      : document.createElement("input");
    const attachedImeProxy = surfaceOptions.textInputElement ?? createdImeProxy;
    if (!attachedImeProxy) throw new Error("Text input proxy was not created");
    this.display = attachedCanvas;
    this.inputElement = attachedInput;
    this.imeProxy = attachedImeProxy;
    if (createdImeProxy) {
      createdImeProxy.type = "text";
      createdImeProxy.autocomplete = "off";
      createdImeProxy.setAttribute("aria-label", "Remote text input");
      Object.assign(createdImeProxy.style, {
        position: "fixed",
        width: "1px",
        height: "1px",
        opacity: "0",
        pointerEvents: "none",
        left: "0",
        bottom: "0",
      });
      document.body.append(createdImeProxy);
    }

    this.context = attachedCanvas.getContext("2d", { alpha: false });
    if (!this.context) {
      createdImeProxy?.remove();
      this.display = null;
      this.inputElement = null;
      this.imeProxy = null;
      throw new Error("The attached canvas does not provide a 2D this.context");
    }

    this.video.attach(attachedCanvas, this.context);
    this.localCursor = new LocalCursor(attachedInput);
    this.clipboardAutoSync = Boolean(surfaceOptions.clipboardAutoSync);
    this.owner.setControlOnFocus(Boolean(surfaceOptions.controlOnFocus));
    if (surfaceOptions.remoteDisplay) {
      this.owner.setRemoteDisplayPolicy(surfaceOptions.remoteDisplay);
    }

    this.inputRuntime.attach(attachedInput, attachedImeProxy);
    this.refreshResizeObservation();

    let surfaceDisposed = false;
    const disposeSurface = () => {
      if (surfaceDisposed) return;
      surfaceDisposed = true;
      this.releaseControl();
      this.localCursor?.dispose();
      this.localCursor = null;
      if (document.pointerLockElement === attachedCanvas)
        document.exitPointerLock();
      this.surfaceListeners.dispose();
      this.inputRuntime.dispose();
      this.resizeObserver?.disconnect();
      this.resizeObserver = null;
      window.removeEventListener("resize", this.scheduleResizeBound);
      if (this.resizeTimer !== null) {
        clearTimeout(this.resizeTimer);
        this.resizeTimer = null;
      }
      this.resizePending = false;
      createdImeProxy?.remove();
      this.display = null;
      this.inputElement = null;
      this.imeProxy = null;
      this.context = null;
      this.video.detach();
      if (this.surfaceDisposer === disposeSurface) this.surfaceDisposer = null;
    };
    this.surfaceDisposer = disposeSurface;
    return Object.freeze({
      requestPointerLock() {
        if (surfaceDisposed) throw new Error("The surface has been disposed");
        return Promise.resolve(attachedCanvas.requestPointerLock());
      },
      exitPointerLock() {
        if (document.pointerLockElement === attachedCanvas)
          document.exitPointerLock();
      },
      focus() {
        if (surfaceDisposed) throw new Error("The surface has been disposed");
        attachedInput.focus({ preventScroll: true });
      },
      focusTextInput() {
        if (surfaceDisposed) throw new Error("The surface has been disposed");
        attachedImeProxy.focus({ preventScroll: true });
      },
      dispose: disposeSurface,
    });
  }

  public connect(): void {
    if (this.sessionDisposed) {
      throw new Error("The session has been disposed");
    }
    if (this.sessionConnected) return;
    if (!this.display || !this.context) {
      throw new Error("Attach a canvas before connecting the session");
    }
    if (!("VideoDecoder" in window)) {
      const error = new Error(
        "This browser does not provide the WebCodecs VideoDecoder API",
      );
      this.owner.updateState("video", {
        state: "error",
        message: error.message,
      });
      this.emit("error", error);
      return;
    }
    this.sessionConnected = true;
    this.connectionGeneration += 1;
    this.video.resetFeedbackInterval();
    void this.connectControl();
    void this.video.connect();
    this.refreshResizeObservation();
    this.feedbackTimer = setInterval(() => this.sendFeedback(), 1000);
  }

  public disconnect(): void {
    const wasConnected = this.sessionConnected;
    this.sessionConnected = false;
    this.connectionGeneration += 1;
    this.controlConnectAttempt = null;
    this.video.invalidateConnectionAttempt();
    this.releaseControl();
    this.resizeObserver?.disconnect();
    this.resizeObserver = null;
    window.removeEventListener("resize", this.scheduleResizeBound);
    if (this.resizeTimer !== null) {
      clearTimeout(this.resizeTimer);
      this.resizeTimer = null;
    }
    this.resizePending = false;
    if (this.feedbackTimer !== null) {
      clearInterval(this.feedbackTimer);
      this.feedbackTimer = null;
    }
    for (const timer of [this.controlReconnectTimer, this.controlStableTimer]) {
      if (timer !== null) clearTimeout(timer);
    }
    this.controlReconnectTimer = null;
    this.controlStableTimer = null;
    this.video.disconnect();
    this.controlSocket?.close(1000, "client this.disconnect");
    this.controlSocket = null;
    this.inputRuntime.cancelAnimation();
    if (wasConnected) {
      this.owner.updateState("video", {
        state: "disconnected",
        message: "Video disconnected",
      });
      this.owner.updateState("input", {
        state: "disconnected",
        message: "Input disconnected",
        connected: false,
      });
    }
  }

  public dispose(): Promise<void> {
    if (this.disposePromise) return this.disposePromise;
    this.sessionDisposed = true;
    this.disposePromise = (async () => {
      this.surfaceDisposer?.();
      this.disconnect();
      if (this.pendingClipboardCopy) {
        const transaction = this.pendingClipboardCopy;
        this.pendingClipboardCopy = null;
        clearTimeout(transaction.timeout);
        transaction.reject(new Error("The session has been disposed"));
      }
      if (this.shortcutClipboardCopy)
        clearTimeout(this.shortcutClipboardCopy.timeout);
      this.shortcutClipboardCopy = null;
      this.pings.clear();
      this.resizeRequests.clear();
    })();
    return this.disposePromise;
  }
  public setLatencyTarget(milliseconds: number): void {
    this.video.setLatencyTarget(milliseconds);
  }
}
