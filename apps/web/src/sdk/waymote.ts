// Framework-independent browser SDK for a Waymote streaming gateway.
import { Either, Schema } from "effect";

export interface RemoteDisplayManualPolicy {
  readonly mode: "manual";
}

export interface RemoteDisplayFixedPolicy {
  readonly mode: "fixed";
  readonly width: number;
  readonly height: number;
  readonly scale: number;
}

export interface RemoteDisplayObservePolicy {
  readonly mode: "observe";
  readonly element?: Element;
  readonly devicePixelRatio?: number;
  readonly debounceMs?: number;
  readonly minWidth?: number;
  readonly minHeight?: number;
  readonly maxWidth?: number;
  readonly maxHeight?: number;
  readonly maxPixels?: number;
}

export type RemoteDisplayPolicy =
  | RemoteDisplayManualPolicy
  | RemoteDisplayFixedPolicy
  | RemoteDisplayObservePolicy;

type TransportPath = "/stream" | "/control";

export interface WaymoteSessionOptions {
  readonly endpoint?: string | URL;
  readonly latency?: number;
  readonly createWebSocket?: (
    path: TransportPath,
    url: URL,
  ) => WebSocket | Promise<WebSocket>;
  readonly remoteDisplay?: RemoteDisplayPolicy;
  /** Minimum time between stats snapshots. Defaults to one snapshot per presented frame. */
  readonly statsIntervalMs?: number;
}

export interface VideoState {
  readonly state:
    | "idle"
    | "connecting"
    | "connected"
    | "reconnecting"
    | "disconnected"
    | "error";
  readonly message: string;
  readonly codec: string | null;
}

export interface InputState {
  readonly state:
    | "idle"
    | "connecting"
    | "requesting"
    | "ready"
    | "active"
    | "busy"
    | "disconnected";
  readonly message: string;
  readonly connected: boolean;
  readonly pointerLocked: boolean;
}

export interface WaymoteSessionState {
  readonly video: VideoState;
  readonly input: InputState;
}

export interface WaymoteStats {
  readonly width: number;
  readonly height: number;
  readonly renderedFps: number;
  /** Media timestamp of the frame most recently drawn, in microseconds. */
  readonly renderedMediaTimestampMicros: number;
  /** Window performance timestamp captured after that draw completed. */
  readonly drawCompletedAtMs: number;
  readonly generation: number;
  readonly bitrateKbps: number;
  readonly scalePercent: number;
  readonly rttMs: number;
  readonly clockConfident: boolean;
  readonly clockUncertaintyMs: number | null;
  readonly latencyTargetMs: number;
  readonly latenessMs: number;
  readonly pendingInputCount: number;
  readonly decoderQueue: number;
  /** Monotonic video diagnostics for this session. */
  readonly receivedFrames: number;
  readonly decodedFrames: number;
  readonly presentedFrames: number;
  readonly droppedFrames: number;
  readonly overdueDroppedFrames: number;
  readonly decodedOverflowDroppedFrames: number;
  readonly decoderResetDroppedFrames: number;
  readonly decoderResets: number;
  readonly resizeState: "idle" | "requested" | "applied" | "presented";
}

export interface ClipboardUpdateEvent {
  readonly text: string | null;
  readonly status: string;
}

export interface ResizeEvent {
  readonly state: "requested" | "applied" | "presented";
  readonly latencyMs?: number;
  readonly width: number;
  readonly height: number;
  readonly scale?: number;
  readonly generation?: number;
}

export interface QualityEvent {
  readonly bitrateKbps: number;
  readonly frameRate: number;
  readonly scalePercent: number;
}

export interface WaymoteEventMap {
  readonly state: WaymoteSessionState;
  readonly stats: WaymoteStats;
  readonly clipboard: ClipboardUpdateEvent;
  readonly resize: ResizeEvent;
  readonly quality: QualityEvent;
  readonly error: Error;
}

export interface SurfaceOptions {
  readonly canvas: HTMLCanvasElement;
  readonly inputElement?: HTMLElement;
  readonly textInputElement?: HTMLInputElement | HTMLTextAreaElement;
  readonly remoteDisplay?: RemoteDisplayPolicy;
  readonly controlOnFocus?: boolean;
  readonly clipboardAutoSync?: boolean;
}

export interface SurfaceHandle {
  requestPointerLock(): Promise<void>;
  exitPointerLock(): void;
  focus(): void;
  focusTextInput(): void;
  dispose(): void;
}

export interface VideoController {
  setLatencyTarget(milliseconds: number): void;
}

export interface InputController {
  acquire(): void;
  release(): void;
}

export interface ClipboardController {
  sendText(text: string): boolean;
  readonly latestRemoteText: string | null;
}

export interface RemoteDisplayController {
  setPolicy(policy: RemoteDisplayPolicy): void;
  manual(): void;
  fixed(configuration: Omit<RemoteDisplayFixedPolicy, "mode">): void;
  observe(configuration?: Omit<RemoteDisplayObservePolicy, "mode">): void;
  readonly policy: RemoteDisplayPolicy;
}

type CursorState = {
  readonly type: "cursor";
  readonly visible: boolean;
  readonly width: number;
  readonly height: number;
  readonly hotspotX: number;
  readonly hotspotY: number;
  readonly image: string;
};

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
type VideoConfiguration = {
  readonly type: "video-config";
  readonly codec: string;
};
const ControlMessageSchema = Schema.Union(
  Schema.Struct({
    type: Schema.Literal("control-state"),
    state: Schema.Literal("active", "busy", "ready"),
  }),
  Schema.Struct({
    type: Schema.Literal("pong"),
    id: Schema.Number,
    serverNanos: Schema.Union(Schema.String, Schema.Number),
  }),
  Schema.Struct({
    type: Schema.Literal("resize-applied"),
    request: Schema.Number,
    width: Schema.Number,
    height: Schema.Number,
    scale: Schema.Number,
    generation: Schema.Number,
  }),
  Schema.Struct({
    type: Schema.Literal("cursor"),
    visible: Schema.Boolean,
    width: Schema.Number,
    height: Schema.Number,
    hotspotX: Schema.Number,
    hotspotY: Schema.Number,
    image: Schema.String,
  }),
  Schema.Struct({ type: Schema.Literal("clipboard"), text: Schema.String }),
  Schema.Struct({
    type: Schema.Literal("quality"),
    bitrate: Schema.Number,
    fps: Schema.Number,
    scale: Schema.Number,
  }),
);
type ControlMessage = typeof ControlMessageSchema.Type;

const VideoConfigurationSchema = Schema.Struct({
  type: Schema.Literal("video-config"),
  codec: Schema.String,
});
function decodeJson<A, I>(
  schema: Schema.Schema<A, I, never>,
  text: string,
): A | null {
  const decoded = Schema.decodeUnknownEither(Schema.parseJson(schema))(text);
  return Either.isRight(decoded) ? decoded.right : null;
}

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
  remoteDisplayPolicy(): RemoteDisplayPolicy;
  controlOnFocus(): boolean;
  setControlOnFocus(enabled: boolean): void;
  setRemoteDisplayPolicy(policy: RemoteDisplayPolicy): void;
};
function toError(cause: unknown): Error {
  return cause instanceof Error ? cause : new Error(String(cause));
}

type Timer = ReturnType<typeof setTimeout>;

const headerSize = 40;
const automaticResizeAlignment = 2;
const minimumOutputWidth = 320;
const maximumOutputWidth = 6000;
const minimumOutputHeight = 180;
const maximumOutputHeight = 6000;
const maximumOutputPixels = maximumOutputWidth * maximumOutputHeight;
const defaultAutomaticOutputPixels = 3840 * 2160;

function fitObservedResize(
  rawWidth: number,
  rawHeight: number,
  policy: Omit<RemoteDisplayObservePolicy, "mode"> = {},
): Readonly<{ width: number; height: number; downscale: number }> {
  const minWidth = Math.min(
    maximumOutputWidth,
    policy.minWidth ?? minimumOutputWidth,
  );
  const minHeight = Math.min(
    maximumOutputHeight,
    policy.minHeight ?? minimumOutputHeight,
  );
  const maxWidth = Math.max(
    minWidth,
    Math.min(maximumOutputWidth, policy.maxWidth ?? maximumOutputWidth),
  );
  const maxHeight = Math.max(
    minHeight,
    Math.min(maximumOutputHeight, policy.maxHeight ?? maximumOutputHeight),
  );
  const maxPixels = Math.min(
    maximumOutputPixels,
    policy.maxPixels ?? defaultAutomaticOutputPixels,
  );
  if (minWidth * minHeight > maxPixels) {
    throw new RangeError("Remote display minimum size exceeds maxPixels");
  }
  const downscale = Math.min(
    1,
    maxWidth / rawWidth,
    maxHeight / rawHeight,
    Math.sqrt(maxPixels / (rawWidth * rawHeight)),
  );
  // Match streamd's even dimensions. H.264 pads its own macroblocks; rounding
  // the desktop to 16 pixels needlessly turns 1080 lines into 1072.
  // sendResize separately suppresses small layout oscillations.
  const width = Math.max(
    minWidth,
    Math.min(
      maxWidth,
      Math.floor((rawWidth * downscale) / automaticResizeAlignment) *
        automaticResizeAlignment,
    ),
  );
  const height = Math.max(
    minHeight,
    Math.min(
      maxHeight,
      Math.floor((rawHeight * downscale) / automaticResizeAlignment) *
        automaticResizeAlignment,
    ),
  );
  return Object.freeze({ width, height, downscale });
}

function normalizeResizeDimensions(
  width: number,
  height: number,
): Readonly<{ width: number; height: number }> {
  return Object.freeze({
    width: Math.max(
      minimumOutputWidth,
      Math.min(maximumOutputWidth, Math.round(width / 2) * 2),
    ),
    height: Math.max(
      minimumOutputHeight,
      Math.min(maximumOutputHeight, Math.round(height / 2) * 2),
    ),
  });
}

// Cursor position belongs to the browser. Only its image comes from the server.
class LocalCursor {
  private readonly element: HTMLElement;
  private readonly scale: () => Scale;
  private readonly onError: (error: Error) => void;
  private readonly original: string;
  private readonly priority: string;
  private active = false;
  private disposed = false;
  private version = 0;
  private state: CursorState | null = null;
  private bitmap: HTMLImageElement | null = null;
  private imageURL = "";
  private cachedBitmap: HTMLImageElement | null = null;
  private cachedKey = "";
  private cachedCSS = "";

  constructor(
    element: HTMLElement,
    scale: () => Scale,
    onError: (error: Error) => void,
  ) {
    this.element = element;
    this.scale = scale;
    this.onError = onError;
    this.original = element.style.getPropertyValue("cursor");
    this.priority = element.style.getPropertyPriority("cursor");
  }

  restore() {
    if (this.original)
      this.element.style.setProperty("cursor", this.original, this.priority);
    else this.element.style.removeProperty("cursor");
  }

  setActive(active: boolean): void {
    this.active = active;
    this.refresh();
  }

  clear(): void {
    this.version++;
    this.active = false;
    this.state = null;
    this.bitmap = null;
    this.imageURL = "";
    this.cachedBitmap = null;
    this.restore();
  }

  dispose(): void {
    this.clear();
    this.disposed = true;
  }

  update(state: CursorState): void {
    if (this.disposed) return;
    const integers = [
      state.width,
      state.height,
      state.hotspotX,
      state.hotspotY,
    ];
    if (
      typeof state.visible !== "boolean" ||
      !integers.every(Number.isSafeInteger) ||
      typeof state.image !== "string" ||
      state.image.length > 400000 ||
      (state.image !== "" &&
        (!/^data:image\/png;base64,[A-Za-z0-9+/=]+$/.test(state.image) ||
          state.width < 1 ||
          state.height < 1 ||
          state.width > 256 ||
          state.height > 256))
    ) {
      throw new TypeError("Invalid remote cursor image");
    }
    this.state = state;
    const version = ++this.version;
    this.refresh();
    if (!state.image || (this.bitmap && this.imageURL === state.image)) return;
    const bitmap = new Image();
    bitmap.src = state.image;
    bitmap
      .decode()
      .then(() => {
        if (this.disposed || version !== this.version) return;
        if (
          bitmap.naturalWidth !== state.width ||
          bitmap.naturalHeight !== state.height
        ) {
          throw new Error("Remote cursor dimensions do not match its image");
        }
        this.bitmap = bitmap;
        this.imageURL = state.image;
        this.refresh();
      })
      .catch((error) => {
        if (this.disposed || version !== this.version) return;
        this.state = null;
        this.restore();
        this.onError(toError(error));
      });
  }

  refresh(): void {
    if (this.disposed) return;
    const state = this.state;
    if (!this.active || !state || !state.image) {
      this.restore();
      return;
    }
    if (!state.visible) {
      this.element.style.setProperty("cursor", "none");
      return;
    }
    if (!this.bitmap || this.imageURL !== state.image) {
      this.restore();
      return;
    }
    const scale = this.scale();
    if (!(scale.x > 0 && scale.y > 0)) {
      this.restore();
      return;
    }
    // Chromium and Firefox limit custom cursors to 128 CSS pixels.
    const limit = Math.min(
      1,
      128 / (state.width * scale.x),
      128 / (state.height * scale.y),
    );
    const width = Math.max(1, Math.round(state.width * scale.x * limit));
    const height = Math.max(1, Math.round(state.height * scale.y * limit));
    const x = Math.max(
      0,
      Math.min(width - 1, Math.round((state.hotspotX * width) / state.width)),
    );
    const y = Math.max(
      0,
      Math.min(
        height - 1,
        Math.round((state.hotspotY * height) / state.height),
      ),
    );
    const key = `${width},${height},${x},${y}`;
    if (this.cachedBitmap !== this.bitmap || this.cachedKey !== key) {
      const canvas = document.createElement("canvas");
      canvas.width = width;
      canvas.height = height;
      const context = canvas.getContext("2d");
      if (!context)
        throw new Error("Cursor canvas does not provide a 2D context");
      context.drawImage(this.bitmap, 0, 0, width, height);
      this.cachedCSS = `url("${canvas.toDataURL("image/png")}") ${x} ${y}, default`;
      this.cachedBitmap = this.bitmap;
      this.cachedKey = key;
    }
    this.element.style.setProperty("cursor", this.cachedCSS);
  }
}

function createRuntime(owner: RuntimeOwner, options: WaymoteSessionOptions) {
  let display: HTMLCanvasElement | null = null;
  let inputElement: HTMLElement | null = null;
  let localCursor: LocalCursor | null = null;
  let imeProxy: HTMLInputElement | HTMLTextAreaElement | null = null;
  let context: CanvasRenderingContext2D | null = null;
  let surfaceCleanup: Array<() => void> = [];
  let resizeObserver: ResizeObserver | null = null;
  let feedbackTimer: Timer | null = null;
  let clipboardAutoSync = false;
  let sessionConnected = false;
  let sessionDisposed = false;
  let connectionGeneration = 0;
  let surfaceDisposer: (() => void) | null = null;
  let disposePromise: Promise<void> | null = null;

  let decoder: VideoDecoder | null = null;
  let videoSocket: WebSocket | null = null;
  const maximumPendingVideoFrames = 24;
  // decodeQueueSize counts compressed packets, not presentation latency. Six
  // packets can be a healthy burst, so match its hard limit to the output cap.
  const maximumVideoDecodeQueueSize = maximumPendingVideoFrames;
  const pendingFrames: VideoFrame[] = [];
  let animationPending = false;
  let animationFrame: number | null = null;
  let presentationTimer: Timer | null = null;
  let renderedFrames = 0;
  let presentedFrames = 0;
  let intervalPresentedFrames = 0;
  let decodedFrames = 0;
  let reconnectDelay = 250;
  let videoReconnectTimer: Timer | null = null;
  let videoConnectAttempt: ConnectionAttempt | null = null;
  let decoderConfiguration: VideoDecoderConfig | null = null;
  let waitingForKeyframe = true;
  let decoderGeneration = 0;
  let receivedChunks = 0;
  let intervalReceivedFrames = 0;
  let receivedKeyframes = 0;
  let queuePeak = 0;
  const busyDecodeQueueSize = 5;
  let queueBusyMilliseconds = 0;
  let queueObservedSize = 0;
  let queueObservedAt = 0;
  let feedbackSampleStartedAt = 0;
  const renderedFrameTimes: number[] = [];

  const clockMaximumAgeMilliseconds = 5000;
  let targetLatencyMilliseconds = Number(options.latency ?? 60);
  let clockOffsetMicros: number | null = null;
  let clockBestRTT = Number.POSITIVE_INFINITY;
  let clockSampleCount = 0;
  let clockLastSample = 0;
  let videoLateness = 0;
  const statsIntervalMilliseconds = Number(options.statsIntervalMs ?? 0);
  let lastStatsPublishedAt = Number.NEGATIVE_INFINITY;
  if (
    !Number.isFinite(targetLatencyMilliseconds) ||
    targetLatencyMilliseconds < 0
  ) {
    throw new RangeError("Latency must be a non-negative number");
  }
  if (
    !Number.isFinite(statsIntervalMilliseconds) ||
    statsIntervalMilliseconds < 0
  ) {
    throw new RangeError("Stats interval must be a non-negative number");
  }

  const controlRecordSize = 16;
  const pointerExtent = 65_535;
  const controlPointerMotion = 1;
  const controlPointerButton = 2;
  const controlPointerScroll = 3;
  const controlKeyboardKey = 4;
  const controlReleaseAll = 5;
  const controlResize = 6;
  const controlPointerRelative = 8;
  const maximumClipboardBytes = 1024 * 1024;
  const clipboardCopyTimeoutMilliseconds = 2000;
  const keyReleased = 0;
  const keyPressed = 1;
  const keyRepeated = 2;
  const linuxPointerButtons = [0x110, 0x112, 0x111, 0x113, 0x114];
  const linuxKeyCodes = new Map([
    ["Escape", 1],
    ["Digit1", 2],
    ["Digit2", 3],
    ["Digit3", 4],
    ["Digit4", 5],
    ["Digit5", 6],
    ["Digit6", 7],
    ["Digit7", 8],
    ["Digit8", 9],
    ["Digit9", 10],
    ["Digit0", 11],
    ["Minus", 12],
    ["Equal", 13],
    ["Backspace", 14],
    ["Tab", 15],
    ["KeyQ", 16],
    ["KeyW", 17],
    ["KeyE", 18],
    ["KeyR", 19],
    ["KeyT", 20],
    ["KeyY", 21],
    ["KeyU", 22],
    ["KeyI", 23],
    ["KeyO", 24],
    ["KeyP", 25],
    ["BracketLeft", 26],
    ["BracketRight", 27],
    ["Enter", 28],
    ["ControlLeft", 29],
    ["KeyA", 30],
    ["KeyS", 31],
    ["KeyD", 32],
    ["KeyF", 33],
    ["KeyG", 34],
    ["KeyH", 35],
    ["KeyJ", 36],
    ["KeyK", 37],
    ["KeyL", 38],
    ["Semicolon", 39],
    ["Quote", 40],
    ["Backquote", 41],
    ["ShiftLeft", 42],
    ["Backslash", 43],
    ["KeyZ", 44],
    ["KeyX", 45],
    ["KeyC", 46],
    ["KeyV", 47],
    ["KeyB", 48],
    ["KeyN", 49],
    ["KeyM", 50],
    ["Comma", 51],
    ["Period", 52],
    ["Slash", 53],
    ["ShiftRight", 54],
    ["NumpadMultiply", 55],
    ["AltLeft", 56],
    ["Space", 57],
    ["CapsLock", 58],
    ["F1", 59],
    ["F2", 60],
    ["F3", 61],
    ["F4", 62],
    ["F5", 63],
    ["F6", 64],
    ["F7", 65],
    ["F8", 66],
    ["F9", 67],
    ["F10", 68],
    ["NumLock", 69],
    ["ScrollLock", 70],
    ["Numpad7", 71],
    ["Numpad8", 72],
    ["Numpad9", 73],
    ["NumpadSubtract", 74],
    ["Numpad4", 75],
    ["Numpad5", 76],
    ["Numpad6", 77],
    ["NumpadAdd", 78],
    ["Numpad1", 79],
    ["Numpad2", 80],
    ["Numpad3", 81],
    ["Numpad0", 82],
    ["NumpadDecimal", 83],
    ["IntlBackslash", 86],
    ["F11", 87],
    ["F12", 88],
    ["NumpadEnter", 96],
    ["ControlRight", 97],
    ["NumpadDivide", 98],
    ["PrintScreen", 99],
    ["AltRight", 100],
    ["Home", 102],
    ["ArrowUp", 103],
    ["PageUp", 104],
    ["ArrowLeft", 105],
    ["ArrowRight", 106],
    ["End", 107],
    ["ArrowDown", 108],
    ["PageDown", 109],
    ["Insert", 110],
    ["Delete", 111],
    ["AudioVolumeMute", 113],
    ["AudioVolumeDown", 114],
    ["AudioVolumeUp", 115],
    ["Power", 116],
    ["NumpadEqual", 117],
    ["Pause", 119],
    ["MetaLeft", 125],
    ["MetaRight", 126],
    ["ContextMenu", 127],
    ["BrowserStop", 128],
    ["Again", 129],
    ["Props", 130],
    ["Undo", 131],
    ["Copy", 133],
    ["Open", 134],
    ["Paste", 135],
    ["Find", 136],
    ["Cut", 137],
    ["Help", 138],
    ["Menu", 139],
    ["Sleep", 142],
    ["WakeUp", 143],
    ["BrowserFavorites", 156],
    ["BrowserBack", 158],
    ["BrowserForward", 159],
    ["Eject", 161],
    ["MediaTrackNext", 163],
    ["MediaPlayPause", 164],
    ["MediaTrackPrevious", 165],
    ["MediaStop", 166],
    ["BrowserRefresh", 173],
    ["BrowserHome", 172],
    ["F13", 183],
    ["F14", 184],
    ["F15", 185],
    ["F16", 186],
    ["F17", 187],
    ["F18", 188],
    ["F19", 189],
    ["F20", 190],
    ["F21", 191],
    ["F22", 192],
    ["F23", 193],
    ["F24", 194],
  ]);

  let controlSocket: WebSocket | null = null;
  let controlReconnectDelay = 250;
  let controlReconnectTimer: Timer | null = null;
  let controlStableTimer: Timer | null = null;
  let controlConnectAttempt: ConnectionAttempt | null = null;
  let controlAcquireDelay = 250;
  let controlAcquireTimer: Timer | null = null;
  let controlActive = false;
  let controlWanted = false;
  let pendingPointerPosition: PointerPosition | null = null;
  let pointerAnimationPending = false;
  let pointerAnimationFrame: number | null = null;
  let pendingControlRecords: ArrayBuffer[] = [];
  let resizeTimer: Timer | null = null;
  let resizePending = false;
  let lastResizeRequest: string | null = null;
  let resizeRequestID = 0;
  let qualityBitrate = 0;
  let qualityScale = 100;
  let remoteClipboard: string | null = null;
  let clipboardPastePending = false;
  let pendingClipboardCopy: PendingClipboardCopy | null = null;
  let shortcutClipboardCopy: ShortcutClipboardCopy | null = null;
  let inputSequence = 0;
  let currentGeneration = 0;
  let latestAppliedInput = 0;
  let droppedFrames = 0;
  let intervalDroppedFrames = 0;
  let overdueDroppedFrames = 0;
  let decodedOverflowDroppedFrames = 0;
  let decoderResetDroppedFrames = 0;
  let decoderResets = 0;
  let rtt = 0;
  let pingID = 0;
  const pings = new Map<number, number>();
  const resizeRequests = new Map<number, ResizeRequest>();
  let resizeState: WaymoteStats["resizeState"] = "idle";
  const pressedKeys = new Set<number>();
  const pressedButtons = new Set<number>();
  let physicalTextPending = false;
  let suppressCompositionText: string | null = null;
  let compositionTimer: Timer | null = null;
  function emit<K extends keyof WaymoteEventMap>(
    type: K,
    value: WaymoteEventMap[K],
  ): void {
    owner.emit(type, value);
  }

  function websocketURL(path: TransportPath): URL {
    const url = new URL(path, options.endpoint ?? location.origin);
    url.protocol = url.protocol === "https:" ? "wss:" : "ws:";
    return url;
  }

  async function createWebSocket(path: TransportPath): Promise<WebSocket> {
    const url = websocketURL(path);
    const socket = options.createWebSocket
      ? await options.createWebSocket(path, url)
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

  function setStatus(text: string, connected = false): void {
    const state = connected
      ? "connected"
      : text.startsWith("Reconnecting")
        ? "reconnecting"
        : text.includes("connecting") || text === "Connecting"
          ? "connecting"
          : text.includes("error") || text.includes("unavailable")
            ? "error"
            : "disconnected";
    owner.updateState("video", { state, message: text });
  }

  function setControlStatus(text: string, connected = false): void {
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
    owner.updateState("input", { state, message: text, connected });
  }

  function setClipboardStatus(text: string): void {
    emit("clipboard", Object.freeze({ text: remoteClipboard, status: text }));
  }

  function clockSynchronized(): boolean {
    return (
      clockOffsetMicros !== null &&
      clockSampleCount >= 2 &&
      performance.now() - clockLastSample <= clockMaximumAgeMilliseconds
    );
  }

  function updateClock(
    sent: number,
    received: number,
    serverNanos: string | number,
  ): void {
    const serverMicros = Number(serverNanos) / 1000;
    const sampleRTT = received - sent;
    if (
      !Number.isFinite(serverMicros) ||
      serverMicros < 0 ||
      !Number.isFinite(sampleRTT) ||
      sampleRTT < 0 ||
      sampleRTT > 60_000
    ) {
      return;
    }
    const candidateOffset = (sent + received) * 500 - serverMicros;
    if (clockOffsetMicros === null || received - clockLastSample > 30_000) {
      clockOffsetMicros = candidateOffset;
      clockBestRTT = sampleRTT;
      clockSampleCount = 1;
      clockLastSample = received;
    } else if (sampleRTT <= clockBestRTT + Math.max(2, clockBestRTT * 0.25)) {
      const correction = candidateOffset - clockOffsetMicros;
      clockOffsetMicros +=
        Math.abs(correction) > 100_000 ? correction : correction * 0.1;
      clockBestRTT = Math.min(clockBestRTT, sampleRTT);
      clockSampleCount += 1;
      clockLastSample = received;
    }
  }

  function expectedPresentationTime(captureMicros: number): number | null {
    if (!clockSynchronized() || !Number.isFinite(captureMicros)) {
      return null;
    }
    const offset = clockOffsetMicros;
    return offset === null
      ? null
      : (captureMicros + offset) / 1000 + targetLatencyMilliseconds;
  }

  function sendClipboardText(text: string): boolean {
    if (new TextEncoder().encode(text).byteLength > maximumClipboardBytes) {
      setClipboardStatus("Clipboard is too large");
      return false;
    }
    if (
      !controlActive ||
      !controlSocket ||
      controlSocket.readyState !== WebSocket.OPEN
    ) {
      setClipboardStatus("Clipboard needs input control");
      return false;
    }
    controlSocket.send(JSON.stringify({ type: "clipboard-write", text }));
    setClipboardStatus("Local clipboard sent");
    return true;
  }

  async function sendLocalClipboard(): Promise<boolean> {
    if (!navigator.clipboard?.readText) {
      throw new Error("Clipboard read is unavailable");
    }
    const text = await navigator.clipboard.readText();
    if (sessionDisposed) return false;
    if (!sendClipboardText(text)) {
      throw new Error("Clipboard control is inactive");
    }
    return true;
  }

  function copyTextFallback(text: string): boolean {
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

  function reserveRemoteClipboardCopy(): boolean {
    if (
      !navigator.clipboard?.write ||
      typeof ClipboardItem === "undefined" ||
      pendingClipboardCopy
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
        if (pendingClipboardCopy !== transaction) return;
        pendingClipboardCopy = null;
        transaction.reject(new Error("Remote copy timed out"));
      }, clipboardCopyTimeoutMilliseconds),
    };
    pendingClipboardCopy = transaction;

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
      pendingClipboardCopy = null;
      transaction.reject(error);
      return false;
    }
    write.then(
      () => {
        if (!sessionDisposed) setClipboardStatus("Remote clipboard copied");
      },
      (error) => {
        if (sessionDisposed) return;
        if (pendingClipboardCopy === transaction) {
          clearTimeout(transaction.timeout);
          pendingClipboardCopy = null;
          transaction.reject(error);
        }
        console.warn("remote shortcut copy failed", error);
        setClipboardStatus("Remote clipboard ready");
      },
    );
    return true;
  }

  function armShortcutClipboardCopy(): void {
    if (shortcutClipboardCopy) {
      clearTimeout(shortcutClipboardCopy.timeout);
    }
    const transaction: ShortcutClipboardCopy = {
      available: false,
      text: "",
      timeout: setTimeout(() => {
        if (shortcutClipboardCopy === transaction) shortcutClipboardCopy = null;
      }, clipboardCopyTimeoutMilliseconds),
    };
    shortcutClipboardCopy = transaction;
  }

  function completeShortcutClipboardCopy(): void {
    const transaction = shortcutClipboardCopy;
    if (!transaction?.available || !copyTextFallback(transaction.text)) {
      return;
    }
    clearTimeout(transaction.timeout);
    shortcutClipboardCopy = null;
    setClipboardStatus("Remote clipboard copied");
  }

  function receiveRemoteClipboard(text: string): void {
    remoteClipboard = text;
    setClipboardStatus(
      text.length === 0 ? "Remote clipboard empty" : "Remote clipboard ready",
    );
    if (shortcutClipboardCopy) {
      shortcutClipboardCopy.text = text;
      shortcutClipboardCopy.available = true;
    }
    if (pendingClipboardCopy) {
      const transaction = pendingClipboardCopy;
      pendingClipboardCopy = null;
      clearTimeout(transaction.timeout);
      transaction.resolve(text);
      return;
    }
    if (text.length === 0) {
      return;
    }
    if (
      clipboardAutoSync &&
      !document.hidden &&
      document.hasFocus() &&
      navigator.clipboard?.writeText
    ) {
      navigator.clipboard.writeText(text).then(
        () => setClipboardStatus("Clipboard synced"),
        () => setClipboardStatus("Remote clipboard ready"),
      );
    }
  }

  function shortcutModifiers(event: KeyboardEvent): number[] {
    const modifiers: number[] = [...pressedKeys].filter(
      (key) =>
        key === 29 ||
        key === 42 ||
        key === 54 ||
        key === 97 ||
        key === 100 ||
        key === 125 ||
        key === 126,
    );
    if (event.ctrlKey && !modifiers.some((key) => key === 29 || key === 97)) {
      modifiers.push(29);
    }
    if (event.shiftKey && !modifiers.some((key) => key === 42 || key === 54)) {
      modifiers.push(42);
    }
    if (event.altKey && !modifiers.includes(56) && !modifiers.includes(100)) {
      modifiers.push(56);
    }
    if (event.metaKey && !modifiers.includes(125) && !modifiers.includes(126)) {
      modifiers.push(125);
    }
    return modifiers;
  }

  function tapRemoteKey(key: number, modifiers: readonly number[]): void {
    for (const modifier of modifiers) {
      sendControl(keyboardRecord(modifier, keyPressed));
    }
    sendControl(keyboardRecord(key, keyPressed));
    sendControl(keyboardRecord(key, keyReleased));
    for (const modifier of modifiers) {
      if (!pressedKeys.has(modifier)) {
        sendControl(keyboardRecord(modifier, keyReleased));
      }
    }
  }

  function updateControlStatus(): void {
    if (!controlSocket || controlSocket.readyState !== WebSocket.OPEN) {
      return;
    }
    if (controlActive) {
      setControlStatus("Input active", true);
    } else if (controlWanted) {
      setControlStatus("Input requesting");
    } else {
      setControlStatus("Input ready · click stream", true);
    }
  }

  function controlRecord(
    type: number,
    pressed = false,
    a = 0,
    b = 0,
    c = 0,
  ): ArrayBuffer {
    const record = new ArrayBuffer(controlRecordSize);
    const view = new DataView(record);
    view.setUint8(0, 2);
    view.setUint8(1, type);
    view.setUint8(2, pressed ? 1 : 0);
    view.setUint32(4, a, true);
    view.setUint32(8, b, true);
    view.setUint32(12, c, true);
    return record;
  }

  function scrollRecord(dx: number, dy: number): ArrayBuffer {
    const record = controlRecord(
      controlPointerScroll,
      false,
      0,
      0,
      nextInputSequence(),
    );
    const view = new DataView(record);
    view.setFloat32(4, dx, true);
    view.setFloat32(8, dy, true);
    return record;
  }

  function keyboardRecord(key: number, state: number): ArrayBuffer {
    const record = controlRecord(
      controlKeyboardKey,
      state === keyPressed,
      key,
      0,
      nextInputSequence(),
    );
    new DataView(record).setUint8(2, state);
    return record;
  }

  function nextInputSequence(): number {
    inputSequence = (inputSequence + 1) >>> 0;
    if (inputSequence === 0) {
      inputSequence = 1;
    }
    return inputSequence;
  }

  function visibleContentBounds(): ContentBounds {
    const currentDisplay = display;
    if (!currentDisplay) throw new Error("No display is attached");
    const bounds = currentDisplay.getBoundingClientRect();
    const contentAspect =
      renderedFrames > 0 &&
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

  function sendResize(): void {
    if (!controlActive) {
      return;
    }
    const policy = owner.remoteDisplayPolicy();
    if (policy.mode === "manual") return;
    if (policy.mode === "fixed") {
      sendResizeDimensions(policy.width, policy.height, policy.scale * 120);
      return;
    }
    const observed = policy.element ?? display;
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
    const previous = lastResizeRequest?.match(/^(\d+)x(\d+)@(\d+)$/);
    if (
      previous &&
      Number(previous[3]) === scale &&
      Math.abs(Number(previous[1]) - width) <= automaticResizeAlignment * 2 &&
      Math.abs(Number(previous[2]) - height) <= automaticResizeAlignment * 2
    ) {
      return;
    }
    sendResizeDimensions(width, height, scale);
  }

  function sendResizeDimensions(
    width: number,
    height: number,
    scale: number,
  ): void {
    ({ width, height } = normalizeResizeDimensions(width, height));
    scale = Math.max(120, Math.min(480, Math.round(scale)));
    const request = `${width}x${height}@${scale}`;
    if (request === lastResizeRequest) {
      return;
    }
    resizeRequestID = (resizeRequestID + 1) & 0xffff;
    if (resizeRequestID === 0) {
      resizeRequestID = 1;
    }
    const packed = (resizeRequestID << 16) | scale;
    const record = controlRecord(
      controlResize,
      false,
      width,
      height,
      packed >>> 0,
    );
    resizeRequests.set((packed >>> 16) & 0xffff, {
      requested: performance.now(),
      generation: 0,
    });
    resizeState = "requested";
    emit(
      "resize",
      Object.freeze({ state: resizeState, width, height, scale: scale / 120 }),
    );
    if (sendControl(record)) {
      lastResizeRequest = request;
    }
  }

  function flushResize(): void {
    resizeTimer = null;
    if (!resizePending) {
      return;
    }
    resizePending = false;
    sendResize();
  }

  function scheduleResize(): void {
    localCursor?.refresh();
    const policy = owner.remoteDisplayPolicy();
    if (policy.mode !== "observe") return;
    if (resizeTimer === null) {
      sendResize();
      resizeTimer = setTimeout(flushResize, policy.debounceMs ?? 100);
      return;
    }
    resizePending = true;
  }

  function sendControl(record: ArrayBuffer): boolean {
    if (!controlSocket) {
      return false;
    }
    if (controlSocket.readyState === WebSocket.CONNECTING) {
      if (pendingControlRecords.length >= 64) {
        pendingControlRecords = [];
        controlSocket.close(4001, "input queue full");
        setControlStatus("Input overloaded");
        return false;
      }
      pendingControlRecords.push(record);
      return true;
    }
    if (controlSocket.readyState !== WebSocket.OPEN) {
      return false;
    }
    controlSocket.send(record);
    return true;
  }

  function releaseInput(): void {
    pendingPointerPosition = null;
    pressedKeys.clear();
    pressedButtons.clear();
    sendControl(controlRecord(controlReleaseAll, false, 0, 0, 0));
  }

  function requestControl(): void {
    controlWanted = true;
    if (controlAcquireTimer !== null) {
      clearTimeout(controlAcquireTimer);
      controlAcquireTimer = null;
    }
    if (sessionConnected) void connectControl();
    if (controlSocket && controlSocket.readyState === WebSocket.OPEN) {
      controlSocket.send("acquire");
      setControlStatus("Input requesting");
    }
  }

  function releaseControl(): void {
    localCursor?.clear();
    controlWanted = false;
    controlActive = false;
    lastResizeRequest = null;
    if (controlAcquireTimer !== null) {
      clearTimeout(controlAcquireTimer);
      controlAcquireTimer = null;
    }
    releaseInput();
    pendingControlRecords = [];
    if (controlSocket && controlSocket.readyState === WebSocket.OPEN) {
      controlSocket.send("release");
      if (sessionConnected)
        setControlStatus("Input ready · click stream", true);
    } else if (sessionConnected) {
      setControlStatus("Input connecting");
    }
  }

  function retryControlAcquire(): void {
    if (
      !controlWanted ||
      !controlSocket ||
      controlSocket.readyState !== WebSocket.OPEN
    ) {
      return;
    }
    controlSocket.send("acquire");
    controlAcquireDelay = Math.min(controlAcquireDelay * 2, 2000);
    controlAcquireTimer = setTimeout(retryControlAcquire, controlAcquireDelay);
  }

  async function connectControl(): Promise<void> {
    if (!sessionConnected || document.hidden) return;
    if (controlReconnectTimer !== null) {
      clearTimeout(controlReconnectTimer);
      controlReconnectTimer = null;
    }
    if (
      controlSocket &&
      (controlSocket.readyState === WebSocket.CONNECTING ||
        controlSocket.readyState === WebSocket.OPEN)
    ) {
      return;
    }
    if (controlConnectAttempt) return;
    setControlStatus("Input connecting");
    const attempt = { generation: connectionGeneration };
    controlConnectAttempt = attempt;
    let socket;
    try {
      socket = await createWebSocket("/control");
    } catch (error) {
      if (controlConnectAttempt !== attempt) return;
      controlConnectAttempt = null;
      if (
        !sessionConnected ||
        document.hidden ||
        attempt.generation !== connectionGeneration
      )
        return;
      const failure = error instanceof Error ? error : new Error(String(error));
      emit("error", failure);
      setControlStatus(`Input reconnecting · ${failure.message}`);
      controlReconnectTimer = setTimeout(() => {
        controlReconnectTimer = null;
        void connectControl();
      }, controlReconnectDelay);
      controlReconnectDelay = Math.min(controlReconnectDelay * 2, 5000);
      return;
    }
    if (
      controlConnectAttempt !== attempt ||
      !sessionConnected ||
      document.hidden ||
      attempt.generation !== connectionGeneration
    ) {
      if (controlConnectAttempt === attempt) controlConnectAttempt = null;
      socket.close(1000, "stale connection attempt");
      return;
    }
    controlConnectAttempt = null;
    socket.binaryType = "arraybuffer";
    controlSocket = socket;
    socket.addEventListener("open", () => {
      if (controlSocket !== socket) {
        socket.close();
        return;
      }
      resetFeedbackInterval();
      if (controlWanted) {
        socket.send("acquire");
      }
      for (const record of pendingControlRecords) {
        sendControl(record);
      }
      pendingControlRecords = [];
      updateControlStatus();
      if (controlStableTimer !== null) clearTimeout(controlStableTimer);
      controlStableTimer = setTimeout(() => {
        controlStableTimer = null;
        if (controlSocket === socket && socket.readyState === WebSocket.OPEN) {
          controlReconnectDelay = 250;
        }
      }, 5000);
    });
    socket.addEventListener("message", (event) => {
      if (controlSocket !== socket || typeof event.data !== "string") {
        return;
      }
      const message: ControlMessage | null = decodeJson(
        ControlMessageSchema,
        event.data,
      );
      if (!message) {
        socket.close(1003, "invalid control state");
        return;
      }
      if (message.type !== "control-state") {
        if (message.type === "pong") {
          const sent = pings.get(message.id);
          if (sent !== undefined) {
            const received = performance.now();
            rtt = received - sent;
            pings.delete(message.id);
            updateClock(sent, received, message.serverNanos);
          }
        }
        if (message.type === "resize-applied") {
          const request = resizeRequests.get(message.request);
          if (request) {
            request.generation = message.generation;
            resizeState = "applied";
            emit(
              "resize",
              Object.freeze({
                state: resizeState,
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
            localCursor?.update(message);
          } catch {
            socket.close(1003, "invalid cursor image");
          }
        }
        if (message.type === "clipboard") receiveRemoteClipboard(message.text);
        if (message.type === "quality") {
          qualityBitrate = message.bitrate;
          qualityScale = message.scale;
          localCursor?.refresh();
          emit(
            "quality",
            Object.freeze({
              bitrateKbps: qualityBitrate,
              frameRate: message.fps,
              scalePercent: qualityScale,
            }),
          );
        }
        return;
      }
      const wasControlActive = controlActive;
      controlActive = message.state === "active";
      localCursor?.setActive(controlActive && controlWanted);
      if (controlActive) {
        controlAcquireDelay = 250;
        if (controlAcquireTimer !== null) {
          clearTimeout(controlAcquireTimer);
          controlAcquireTimer = null;
        }
        if (!controlWanted) {
          controlActive = false;
          socket.send("release");
        } else if (!wasControlActive) {
          lastResizeRequest = null;
          sendResize();
        }
      } else if (message.state === "busy" && controlWanted) {
        lastResizeRequest = null;
        pendingPointerPosition = null;
        pressedKeys.clear();
        pressedButtons.clear();
        setControlStatus("Input in use");
        if (controlAcquireTimer === null) {
          controlAcquireTimer = setTimeout(
            retryControlAcquire,
            controlAcquireDelay,
          );
        }
        return;
      } else if (message.state === "ready" && controlWanted) {
        lastResizeRequest = null;
        requestControl();
        return;
      }
      updateControlStatus();
    });
    socket.addEventListener("close", (event) => {
      if (controlSocket !== socket) {
        return;
      }
      controlSocket = null;
      localCursor?.clear();
      controlActive = false;
      lastResizeRequest = null;
      pendingControlRecords = [];
      pressedKeys.clear();
      pressedButtons.clear();
      if (controlAcquireTimer !== null) {
        clearTimeout(controlAcquireTimer);
        controlAcquireTimer = null;
      }
      if (sessionConnected && !document.hidden) {
        setControlStatus(`Input reconnecting · ${event.code}`);
        controlReconnectTimer = setTimeout(() => {
          controlReconnectTimer = null;
          void connectControl();
        }, controlReconnectDelay);
        controlReconnectDelay = Math.min(controlReconnectDelay * 2, 5000);
      } else {
        setControlStatus("Input disconnected");
      }
    });
    socket.addEventListener("error", () => socket.close());
  }

  function contentPosition(event: PointerEvent): PointerPosition | null {
    const currentDisplay = display;
    if (
      !currentDisplay ||
      currentDisplay.width === 0 ||
      currentDisplay.height === 0
    ) {
      return null;
    }
    const { width, height, left, top } = visibleContentBounds();
    const x = (event.clientX - left) / width;
    const y = (event.clientY - top) / height;
    if (x < 0 || x > 1 || y < 0 || y > 1) {
      return null;
    }
    return {
      x: Math.round(x * pointerExtent),
      y: Math.round(y * pointerExtent),
    };
  }

  function queuePointerPosition(event: PointerEvent): void {
    if (document.pointerLockElement === display) {
      return;
    }
    pendingPointerPosition = contentPosition(event);
    if (!pendingPointerPosition || pointerAnimationPending) {
      return;
    }
    pointerAnimationPending = true;
    pointerAnimationFrame = requestAnimationFrame(() => {
      pointerAnimationFrame = null;
      pointerAnimationPending = false;
      const position = pendingPointerPosition;
      pendingPointerPosition = null;
      if (position) {
        sendControl(
          controlRecord(
            controlPointerMotion,
            false,
            position.x,
            position.y,
            nextInputSequence(),
          ),
        );
      }
    });
  }

  function handleRelativePointerMove(event: PointerEvent): void {
    if (document.pointerLockElement !== display) return;
    const dx = Math.max(-4096, Math.min(4096, event.movementX));
    const dy = Math.max(-4096, Math.min(4096, event.movementY));
    const record = controlRecord(
      controlPointerRelative,
      false,
      0,
      0,
      nextInputSequence(),
    );
    const view = new DataView(record);
    view.setFloat32(4, dx, true);
    view.setFloat32(8, dy, true);
    sendControl(record);
  }

  function handlePointerLockChange(): void {
    owner.updateState("input", {
      pointerLocked: document.pointerLockElement === display,
    });
    if (document.pointerLockElement !== display) releaseInput();
  }

  function handlePointerDown(event: PointerEvent): void {
    const locked = document.pointerLockElement === display;
    const position = locked ? null : contentPosition(event);
    if ((!locked && !position) || event.button < 0 || event.button > 4) {
      return;
    }
    const currentInput = inputElement;
    if (!currentInput) return;
    currentInput.focus({ preventScroll: true });
    if (!locked) currentInput.setPointerCapture(event.pointerId);
    if (position) {
      sendControl(
        controlRecord(
          controlPointerMotion,
          false,
          position.x,
          position.y,
          nextInputSequence(),
        ),
      );
    }
    const button = linuxPointerButtons[event.button];
    if (button !== undefined && !pressedButtons.has(event.button)) {
      pressedButtons.add(event.button);
      sendControl(
        controlRecord(
          controlPointerButton,
          true,
          button,
          0,
          nextInputSequence(),
        ),
      );
    }
    event.preventDefault();
  }

  function handlePointerUp(event: PointerEvent): void {
    queuePointerPosition(event);
    const button = linuxPointerButtons[event.button];
    if (button !== undefined && pressedButtons.delete(event.button)) {
      sendControl(
        controlRecord(
          controlPointerButton,
          false,
          button,
          0,
          nextInputSequence(),
        ),
      );
    }
    if (inputElement?.hasPointerCapture(event.pointerId)) {
      inputElement.releasePointerCapture(event.pointerId);
    }
    event.preventDefault();
  }

  function handleWheel(event: WheelEvent): void {
    inputElement?.focus({ preventScroll: true });
    const scale =
      event.deltaMode === WheelEvent.DOM_DELTA_LINE
        ? 16
        : event.deltaMode === WheelEvent.DOM_DELTA_PAGE
          ? (display?.clientHeight ?? 1)
          : 1;
    const dx = Math.max(-4096, Math.min(4096, event.deltaX * scale));
    const dy = Math.max(-4096, Math.min(4096, event.deltaY * scale));
    sendControl(scrollRecord(dx, dy));
    event.preventDefault();
  }

  function handleKeyDown(event: KeyboardEvent): void {
    if (event.isComposing || event.keyCode === 229) {
      return;
    }
    const key = linuxKeyCodes.get(event.code);
    if (key === undefined) {
      return;
    }
    if ((event.ctrlKey || event.metaKey) && event.code === "KeyV") {
      if (!event.repeat && !clipboardPastePending) {
        clipboardPastePending = true;
        const modifiers = shortcutModifiers(event);
        sendLocalClipboard()
          .catch((error) => {
            if (!sessionDisposed) {
              console.warn("local clipboard sync failed", error);
              setClipboardStatus("Local clipboard unavailable");
            }
          })
          .finally(() => {
            if (!sessionDisposed) tapRemoteKey(key, modifiers);
            clipboardPastePending = false;
          });
      }
      event.preventDefault();
      event.stopPropagation();
      return;
    }
    if (
      (event.ctrlKey || event.metaKey) &&
      event.shiftKey &&
      event.code === "KeyC"
    ) {
      if (!event.repeat) {
        armShortcutClipboardCopy();
        reserveRemoteClipboardCopy();
        tapRemoteKey(key, shortcutModifiers(event));
      }
      event.preventDefault();
      event.stopPropagation();
      return;
    }
    if (!pressedKeys.has(key)) {
      pressedKeys.add(key);
      sendControl(keyboardRecord(key, keyPressed));
    } else if (event.repeat) {
      sendControl(keyboardRecord(key, keyRepeated));
    }
    physicalTextPending =
      event.key?.length === 1 && !event.ctrlKey && !event.metaKey;
    event.preventDefault();
    event.stopPropagation();
  }

  function handleKeyUp(event: KeyboardEvent): void {
    if (event.isComposing || event.keyCode === 229) {
      return;
    }
    const key = linuxKeyCodes.get(event.code);
    if (key === undefined) {
      return;
    }
    if (
      event.code === "KeyC" ||
      event.code === "ShiftLeft" ||
      event.code === "ShiftRight" ||
      event.code === "ControlLeft" ||
      event.code === "ControlRight" ||
      event.code === "MetaLeft" ||
      event.code === "MetaRight"
    ) {
      completeShortcutClipboardCopy();
    }
    if (pressedKeys.delete(key)) {
      sendControl(keyboardRecord(key, keyReleased));
    }
    physicalTextPending = false;
    event.preventDefault();
    event.stopPropagation();
  }

  function handleSurfaceFocus(): void {
    if (owner.controlOnFocus()) requestControl();
  }

  function handleSurfaceBlur(event: FocusEvent): void {
    if (event.relatedTarget !== imeProxy) {
      releaseControl();
    }
  }

  function handleVisibilityChange(): void {
    if (document.hidden) {
      releaseControl();
      controlConnectAttempt = null;
      videoConnectAttempt = null;
      videoSocket?.close(1000, "page hidden");
    } else if (sessionConnected) {
      void connectControl();
      if (!videoSocket) void connectVideo();
      if (owner.controlOnFocus() && document.activeElement === inputElement) {
        requestControl();
      }
    }
  }

  async function configureDecoder(
    configuration: VideoConfiguration,
  ): Promise<void> {
    const generation = ++decoderGeneration;
    if (decoder) {
      decoder.close();
      decoder = null;
    }
    decoderConfiguration = {
      codec: configuration.codec,
      optimizeForLatency: true,
    };
    waitingForKeyframe = true;
    renderedFrameTimes.length = 0;
    let support;
    try {
      support = await VideoDecoder.isConfigSupported(decoderConfiguration);
    } catch (error) {
      if (
        generation !== decoderGeneration ||
        sessionDisposed ||
        !sessionConnected
      )
        return;
      throw error;
    }
    if (generation !== decoderGeneration) {
      return;
    }
    if (!support.supported) {
      decoderConfiguration = null;
      setStatus("Unsupported codec");
      owner.updateState("video", {
        state: "error",
        message: `This browser cannot decode ${configuration.codec}.`,
        codec: configuration.codec,
      });
      return;
    }

    const configuredDecoder = new VideoDecoder({
      output(frame) {
        decodedFrames += 1;
        pendingFrames.push(frame);
        if (pendingFrames.length > maximumPendingVideoFrames) {
          pendingFrames.shift()?.close();
          droppedFrames += 1;
          intervalDroppedFrames += 1;
          decodedOverflowDroppedFrames += 1;
        }
        scheduleVideoPresentation();
      },
      error(error) {
        console.error("video decoder error", error);
        if (decoder === configuredDecoder) {
          decoder = null;
        }
        setStatus(`Video decoder error · ${error.message || error.name}`);
        if (videoSocket?.readyState === WebSocket.OPEN) {
          videoSocket.close(4001, "video decoder error");
        }
      },
    });
    configuredDecoder.addEventListener("dequeue", () => {
      if (decoder === configuredDecoder && !sessionDisposed) {
        observeDecodeQueue(configuredDecoder.decodeQueueSize);
      }
    });
    configuredDecoder.configure(decoderConfiguration);
    decoder = configuredDecoder;
    observeDecodeQueue(configuredDecoder.decodeQueueSize);
    owner.updateState("video", { codec: configuration.codec });
  }

  function cancelVideoPresentation(): void {
    if (presentationTimer !== null) {
      clearTimeout(presentationTimer);
      presentationTimer = null;
    }
    if (animationFrame !== null) {
      cancelAnimationFrame(animationFrame);
      animationFrame = null;
    }
    animationPending = false;
  }

  function observeDecodeQueue(size: number, now = performance.now()): void {
    if (queueObservedSize >= busyDecodeQueueSize) {
      queueBusyMilliseconds += Math.max(0, now - queueObservedAt);
    }
    queueObservedSize = size;
    queueObservedAt = now;
    queuePeak = Math.max(queuePeak, size);
  }

  function resetFeedbackInterval(now = performance.now()): void {
    intervalReceivedFrames = 0;
    intervalPresentedFrames = 0;
    intervalDroppedFrames = 0;
    queueBusyMilliseconds = 0;
    feedbackSampleStartedAt = now;
    queueObservedAt = now;
    queueObservedSize = decoder?.decodeQueueSize ?? 0;
    queuePeak = queueObservedSize;
  }

  function resetVideoDecoder(configuredDecoder: VideoDecoder): void {
    const queuedCompressedFrames = configuredDecoder.decodeQueueSize;
    const decodedPendingFrames = pendingFrames.length;
    observeDecodeQueue(queuedCompressedFrames);
    const resetFrames = queuedCompressedFrames + decodedPendingFrames;
    droppedFrames += resetFrames;
    intervalDroppedFrames += resetFrames;
    decoderResetDroppedFrames += resetFrames;
    decoderResets += 1;

    cancelVideoPresentation();
    for (const frame of pendingFrames.splice(0)) frame.close();
    waitingForKeyframe = true;
    configuredDecoder.reset();
    observeDecodeQueue(configuredDecoder.decodeQueueSize);
    if (decoderConfiguration) configuredDecoder.configure(decoderConfiguration);
  }

  function scheduleVideoPresentation(): void {
    if (
      pendingFrames.length === 0 ||
      animationPending ||
      presentationTimer !== null
    ) {
      return;
    }
    const nextFrame = pendingFrames[0];
    if (!nextFrame) return;
    const presentation = expectedPresentationTime(nextFrame.timestamp);
    const delay = presentation === null ? 0 : presentation - performance.now();
    if (delay > 8) {
      presentationTimer = setTimeout(
        () => {
          presentationTimer = null;
          scheduleVideoPresentation();
        },
        Math.min(delay - 4, 1000),
      );
      return;
    }
    animationPending = true;
    animationFrame = requestAnimationFrame(renderFrame);
  }

  function renderFrame(): void {
    animationFrame = null;
    animationPending = false;
    if (pendingFrames.length === 0) {
      return;
    }
    if (!display || !context) {
      for (const frame of pendingFrames.splice(0)) frame.close();
      return;
    }
    const now = performance.now();
    let frame: VideoFrame | undefined;
    if (!clockSynchronized()) {
      frame = pendingFrames.pop();
      for (const stale of pendingFrames.splice(0)) {
        stale.close();
        droppedFrames += 1;
        intervalDroppedFrames += 1;
        overdueDroppedFrames += 1;
      }
    } else {
      let due = 0;
      while (due < pendingFrames.length) {
        const candidate = pendingFrames[due];
        if (!candidate) break;
        const presentation = expectedPresentationTime(candidate.timestamp);
        if (presentation !== null && presentation > now) break;
        due += 1;
      }
      if (due === 0) {
        scheduleVideoPresentation();
        return;
      }
      const ready = pendingFrames.splice(0, due);
      frame = ready.pop();
      for (const stale of ready) {
        stale.close();
        droppedFrames += 1;
        intervalDroppedFrames += 1;
        overdueDroppedFrames += 1;
      }
    }
    if (!frame) return;
    const dimensionsChanged =
      display.width !== frame.displayWidth ||
      display.height !== frame.displayHeight;
    if (dimensionsChanged) {
      display.width = frame.displayWidth;
      display.height = frame.displayHeight;
    }
    context.drawImage(frame, 0, 0, display.width, display.height);
    const drawCompletedAt = performance.now();
    renderedFrames += 1;
    if (dimensionsChanged) localCursor?.refresh();
    const presentation = expectedPresentationTime(frame.timestamp);
    videoLateness = presentation === null ? 0 : now - presentation;
    frame.close();
    presentedFrames += 1;
    intervalPresentedFrames += 1;
    renderedFrameTimes.push(now);
    const windowStart = now - 1000;
    while (
      renderedFrameTimes.length > 1 &&
      (renderedFrameTimes[0] ?? now) < windowStart
    ) {
      renderedFrameTimes.shift();
    }
    if (now - lastStatsPublishedAt >= statsIntervalMilliseconds) {
      lastStatsPublishedAt = now;
      const elapsed = now - (renderedFrameTimes[0] ?? now);
      const renderedFps =
        elapsed > 0 ? ((renderedFrameTimes.length - 1) * 1000) / elapsed : 0;
      owner.setStats({
        width: display.width,
        height: display.height,
        renderedFps,
        renderedMediaTimestampMicros: frame.timestamp,
        drawCompletedAtMs: drawCompletedAt,
        generation: currentGeneration,
        bitrateKbps: qualityBitrate,
        scalePercent: qualityScale,
        rttMs: rtt,
        clockConfident: clockSynchronized(),
        clockUncertaintyMs: Number.isFinite(clockBestRTT)
          ? clockBestRTT / 2
          : null,
        latencyTargetMs: targetLatencyMilliseconds,
        latenessMs: videoLateness,
        pendingInputCount: Math.max(0, inputSequence - latestAppliedInput),
        decoderQueue: decoder?.decodeQueueSize || 0,
        receivedFrames: receivedChunks,
        decodedFrames,
        presentedFrames,
        droppedFrames,
        overdueDroppedFrames,
        decodedOverflowDroppedFrames,
        decoderResetDroppedFrames,
        decoderResets,
        resizeState,
      });
    }
    scheduleVideoPresentation();
  }

  function decodeMessage(buffer: ArrayBuffer): void {
    if (!(buffer instanceof ArrayBuffer) || buffer.byteLength < headerSize) {
      return;
    }
    const view = new DataView(buffer);
    if (view.getUint8(0) !== 2 || view.getUint8(1) !== 1) {
      return;
    }
    const keyframe = (view.getUint8(2) & 1) !== 0;
    const discontinuity = (view.getUint8(2) & 2) !== 0;
    receivedChunks += 1;
    intervalReceivedFrames += 1;
    if (keyframe) {
      receivedKeyframes += 1;
    }
    if (renderedFrames === 0) {
      owner.updateState("video", {
        message: `Receiving video · ${receivedChunks} chunks · ${receivedKeyframes} keyframes`,
      });
    }
    const configuredDecoder = decoder;
    if (!configuredDecoder || configuredDecoder.state !== "configured") {
      return;
    }
    const timestamp = Number(view.getBigUint64(12, true));
    const generation = view.getUint32(20, true);
    latestAppliedInput = view.getUint32(36, true);
    const generationChanged = generation !== currentGeneration;
    if (generationChanged) currentGeneration = generation;
    for (const [id, request] of resizeRequests) {
      if (request.generation === generation) {
        const latencyMs = performance.now() - request.requested;
        resizeState = "presented";
        emit(
          "resize",
          Object.freeze({
            state: resizeState,
            latencyMs,
            width: view.getUint16(24, true),
            height: view.getUint16(26, true),
            generation,
          }),
        );
        resizeRequests.delete(id);
      }
    }

    const queuedBeforeDecode = configuredDecoder.decodeQueueSize;
    observeDecodeQueue(queuedBeforeDecode);
    if (
      generationChanged ||
      discontinuity ||
      queuedBeforeDecode >= maximumVideoDecodeQueueSize
    ) {
      resetVideoDecoder(configuredDecoder);
    }
    if (
      decoder !== configuredDecoder ||
      configuredDecoder.state !== "configured" ||
      (waitingForKeyframe && !keyframe)
    ) {
      return;
    }
    waitingForKeyframe = false;
    configuredDecoder.decode(
      new EncodedVideoChunk({
        type: keyframe ? "key" : "delta",
        timestamp,
        data: new Uint8Array(buffer, headerSize),
      }),
    );
    observeDecodeQueue(configuredDecoder.decodeQueueSize);
  }

  function handleCompositionUpdate(event: CompositionEvent): void {
    if (controlActive) {
      controlSocket?.send(
        JSON.stringify({
          type: "text",
          action: "preedit",
          text: event.data,
          sequence: nextInputSequence(),
        }),
      );
    }
  }

  function handleCompositionEnd(event: CompositionEvent): void {
    if (controlActive) {
      controlSocket?.send(
        JSON.stringify({
          type: "text",
          action: "commit",
          text: event.data,
          sequence: nextInputSequence(),
        }),
      );
    }
    suppressCompositionText = event.data;
    if (compositionTimer !== null) clearTimeout(compositionTimer);
    compositionTimer = setTimeout(() => {
      compositionTimer = null;
      if (suppressCompositionText === event.data) {
        suppressCompositionText = null;
      }
    }, 0);
    if (imeProxy) imeProxy.value = "";
  }

  function handleBeforeInput(event: InputEvent): void {
    if (event.isComposing || event.inputType !== "insertText" || !event.data) {
      return;
    }
    if (physicalTextPending || suppressCompositionText === event.data) {
      physicalTextPending = false;
      suppressCompositionText = null;
      event.preventDefault();
    } else if (controlActive) {
      controlSocket?.send(
        JSON.stringify({
          type: "text",
          action: "commit",
          text: event.data,
          sequence: nextInputSequence(),
        }),
      );
      event.preventDefault();
    }
    if (imeProxy) imeProxy.value = "";
  }

  function handleTextInputFocus(): void {
    requestControl();
  }

  function handleTextInputBlur(event: FocusEvent): void {
    if (controlActive) {
      controlSocket?.send(
        JSON.stringify({
          type: "text",
          action: "preedit",
          text: "",
          sequence: nextInputSequence(),
        }),
      );
    }
    if (imeProxy) imeProxy.value = "";
    if (event.relatedTarget === display) {
      return;
    }
    releaseControl();
  }

  function sendFeedback(): void {
    const now = performance.now();
    observeDecodeQueue(decoder?.decodeQueueSize ?? 0, now);
    const sampleMilliseconds = now - feedbackSampleStartedAt;
    const busyMilliseconds = Math.min(
      Math.max(0, queueBusyMilliseconds),
      Math.max(0, sampleMilliseconds),
    );
    while ((renderedFrameTimes[0] ?? now) < now - 1000) {
      renderedFrameTimes.shift();
    }
    const socket = controlSocket;
    if (socket && socket.readyState === WebSocket.OPEN) {
      const id = ++pingID;
      pings.set(id, now);
      for (const [pendingID, sent] of pings) {
        if (sent < now - 10_000) {
          pings.delete(pendingID);
        }
      }
      socket.send(JSON.stringify({ type: "ping", id }));
      if (
        Number.isFinite(sampleMilliseconds) &&
        sampleMilliseconds > 0 &&
        sampleMilliseconds <= 60_000
      ) {
        socket.send(
          JSON.stringify({
            type: "feedback",
            received: intervalReceivedFrames,
            presented: intervalPresentedFrames,
            queuePeak,
            queueBusyMs: busyMilliseconds,
            sampleMs: sampleMilliseconds,
            dropped: intervalDroppedFrames,
            rtt,
          }),
        );
      }
    }
    resetFeedbackInterval(now);
  }

  async function connectVideo(): Promise<void> {
    if (!sessionConnected || document.hidden) return;
    if (videoReconnectTimer !== null) {
      clearTimeout(videoReconnectTimer);
      videoReconnectTimer = null;
    }
    if (videoConnectAttempt) return;
    setStatus("Connecting");
    const attempt = { generation: connectionGeneration };
    videoConnectAttempt = attempt;
    let socket;
    try {
      socket = await createWebSocket("/stream");
    } catch (error) {
      if (videoConnectAttempt !== attempt) return;
      videoConnectAttempt = null;
      if (
        !sessionConnected ||
        attempt.generation !== connectionGeneration ||
        document.hidden
      )
        return;
      const failure = error instanceof Error ? error : new Error(String(error));
      emit("error", failure);
      setStatus(`Reconnecting · ${failure.message}`);
      videoReconnectTimer = setTimeout(() => {
        videoReconnectTimer = null;
        void connectVideo();
      }, reconnectDelay);
      reconnectDelay = Math.min(reconnectDelay * 2, 5000);
      return;
    }
    if (
      videoConnectAttempt !== attempt ||
      !sessionConnected ||
      document.hidden ||
      attempt.generation !== connectionGeneration
    ) {
      if (videoConnectAttempt === attempt) videoConnectAttempt = null;
      socket.close(1000, "stale connection attempt");
      return;
    }
    videoConnectAttempt = null;
    videoSocket = socket;
    let decoderSetup = Promise.resolve();
    socket.binaryType = "arraybuffer";
    socket.addEventListener("open", () => {
      if (videoSocket !== socket) return;
      reconnectDelay = 250;
      setStatus("Connected", true);
    });
    socket.addEventListener("message", (event) => {
      if (videoSocket !== socket) return;
      if (typeof event.data === "string") {
        const message = decodeJson(VideoConfigurationSchema, event.data);
        if (!message) {
          socket.close(1003, "invalid video configuration");
          return;
        }
        decoderSetup = configureDecoder(message).catch((error) => {
          if (videoSocket !== socket || sessionDisposed || !sessionConnected)
            return;
          console.error("video decoder configuration failed", error);
          setStatus("Video decoder error");
          socket.close(4001, "video decoder configuration failed");
        });
        return;
      }
      if (event.data instanceof ArrayBuffer) {
        const data = event.data;
        void decoderSetup.then(() => {
          if (videoSocket !== socket || sessionDisposed || !sessionConnected)
            return;
          decodeMessage(data);
        });
      }
    });
    socket.addEventListener("close", (event) => {
      if (videoSocket !== socket) return;
      videoSocket = null;
      const reason = event.reason || `WebSocket code ${event.code}`;
      console.warn("video WebSocket closed", event.code, event.reason);
      setStatus(
        sessionConnected ? `Reconnecting · ${event.code}` : "Disconnected",
      );
      if (renderedFrames === 0) {
        owner.updateState("video", { message: `${reason}. Retrying…` });
      }
      decoderGeneration += 1;
      if (decoder) {
        decoder.close();
        decoder = null;
      }
      for (const frame of pendingFrames.splice(0)) {
        frame.close();
      }
      if (presentationTimer !== null) {
        clearTimeout(presentationTimer);
        presentationTimer = null;
      }
      decoderConfiguration = null;
      waitingForKeyframe = true;
      if (sessionConnected && !document.hidden) {
        videoReconnectTimer = setTimeout(() => {
          videoReconnectTimer = null;
          void connectVideo();
        }, reconnectDelay);
      }
      reconnectDelay = Math.min(reconnectDelay * 2, 5000);
    });
    socket.addEventListener("error", () => socket.close());
  }

  function setLatencyTarget(milliseconds: number): void {
    milliseconds = Number(milliseconds);
    if (!Number.isFinite(milliseconds) || milliseconds < 0) {
      throw new RangeError("Latency must be a non-negative number");
    }
    targetLatencyMilliseconds = milliseconds;
    if (presentationTimer !== null) {
      clearTimeout(presentationTimer);
      presentationTimer = null;
    }
    scheduleVideoPresentation();
  }

  function refreshResizeObservation(): void {
    resizeObserver?.disconnect();
    resizeObserver = null;
    if (typeof window === "undefined") return;
    window.removeEventListener("resize", scheduleResize);

    const policy = owner.remoteDisplayPolicy();
    if (!sessionConnected || !display) return;
    const observedDisplay =
      policy.mode === "observe" ? (policy.element ?? display) : display;
    if (typeof ResizeObserver !== "undefined") {
      resizeObserver = new ResizeObserver(scheduleResize);
      resizeObserver.observe(inputElement ?? display);
      if (observedDisplay !== inputElement)
        resizeObserver.observe(observedDisplay);
    }
    if (policy.mode === "observe") {
      window.addEventListener("resize", scheduleResize);
    }
    scheduleResize();
  }

  function remoteDisplayPolicyChanged(): void {
    lastResizeRequest = null;
    if (resizeTimer !== null) {
      clearTimeout(resizeTimer);
      resizeTimer = null;
    }
    resizePending = false;
    refreshResizeObservation();
    sendResize();
  }

  function addSurfaceListener<E extends Event>(
    target: EventTarget,
    type: string,
    listener: (event: E) => void,
    options?: AddEventListenerOptions | boolean,
  ): void {
    const adapted: EventListener = (event) => listener(event as E);
    target.addEventListener(type, adapted, options);
    surfaceCleanup.push(() =>
      target.removeEventListener(type, adapted, options),
    );
  }

  function attachSurface(surfaceOptions: SurfaceOptions): SurfaceHandle {
    if (sessionDisposed) {
      throw new Error("The session has been disposed");
    }
    if (!surfaceOptions?.canvas) {
      throw new TypeError("attachSurface requires a canvas");
    }
    if (display) {
      throw new Error("A surface is already attached to this session");
    }

    const attachedCanvas = surfaceOptions.canvas;
    const attachedInput = surfaceOptions.inputElement ?? attachedCanvas;
    const createdImeProxy = surfaceOptions.textInputElement
      ? null
      : document.createElement("input");
    const attachedImeProxy = surfaceOptions.textInputElement ?? createdImeProxy;
    if (!attachedImeProxy) throw new Error("Text input proxy was not created");
    display = attachedCanvas;
    inputElement = attachedInput;
    imeProxy = attachedImeProxy;
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

    context = attachedCanvas.getContext("2d", { alpha: false });
    if (!context) {
      createdImeProxy?.remove();
      display = null;
      inputElement = null;
      imeProxy = null;
      throw new Error("The attached canvas does not provide a 2D context");
    }

    localCursor = new LocalCursor(
      attachedInput,
      () => {
        if (renderedFrames === 0) return { x: 0, y: 0 };
        const bounds = visibleContentBounds();
        return {
          x: ((bounds.width / attachedCanvas.width) * qualityScale) / 100,
          y: ((bounds.height / attachedCanvas.height) * qualityScale) / 100,
        };
      },
      (error) => emit("error", error),
    );
    addSurfaceListener(window, "resize", () => localCursor?.refresh());
    clipboardAutoSync = Boolean(surfaceOptions.clipboardAutoSync);
    owner.setControlOnFocus(Boolean(surfaceOptions.controlOnFocus));
    if (surfaceOptions.remoteDisplay) {
      owner.setRemoteDisplayPolicy(surfaceOptions.remoteDisplay);
    }

    addSurfaceListener(attachedInput, "pointermove", queuePointerPosition);
    addSurfaceListener(attachedInput, "pointermove", handleRelativePointerMove);
    addSurfaceListener(attachedInput, "pointerdown", handlePointerDown);
    addSurfaceListener(attachedInput, "pointerup", handlePointerUp);
    addSurfaceListener(attachedInput, "pointercancel", releaseInput);
    addSurfaceListener(attachedInput, "contextmenu", (event) =>
      event.preventDefault(),
    );
    addSurfaceListener(attachedInput, "wheel", handleWheel, { passive: false });
    addSurfaceListener(attachedInput, "keydown", handleKeyDown);
    addSurfaceListener(attachedInput, "keyup", handleKeyUp);
    addSurfaceListener(attachedInput, "focus", handleSurfaceFocus);
    addSurfaceListener(attachedInput, "blur", handleSurfaceBlur);
    if (attachedImeProxy !== attachedInput) {
      addSurfaceListener(attachedImeProxy, "keydown", handleKeyDown);
      addSurfaceListener(attachedImeProxy, "keyup", handleKeyUp);
    }
    addSurfaceListener(
      attachedImeProxy,
      "compositionupdate",
      handleCompositionUpdate,
    );
    addSurfaceListener(
      attachedImeProxy,
      "compositionend",
      handleCompositionEnd,
    );
    addSurfaceListener(attachedImeProxy, "beforeinput", handleBeforeInput);
    addSurfaceListener(attachedImeProxy, "focus", handleTextInputFocus);
    addSurfaceListener(attachedImeProxy, "blur", handleTextInputBlur);
    addSurfaceListener(document, "pointerlockchange", handlePointerLockChange);
    addSurfaceListener(document, "pointerlockerror", () => {
      emit("error", new Error("The browser denied pointer lock"));
    });
    addSurfaceListener(document, "visibilitychange", handleVisibilityChange);
    refreshResizeObservation();

    let surfaceDisposed = false;
    const disposeSurface = () => {
      if (surfaceDisposed) return;
      surfaceDisposed = true;
      releaseControl();
      localCursor?.dispose();
      localCursor = null;
      if (document.pointerLockElement === attachedCanvas)
        document.exitPointerLock();
      for (const cleanup of surfaceCleanup.splice(0)) cleanup();
      resizeObserver?.disconnect();
      resizeObserver = null;
      window.removeEventListener("resize", scheduleResize);
      if (resizeTimer !== null) {
        clearTimeout(resizeTimer);
        resizeTimer = null;
      }
      resizePending = false;
      createdImeProxy?.remove();
      display = null;
      inputElement = null;
      imeProxy = null;
      context = null;
      if (surfaceDisposer === disposeSurface) surfaceDisposer = null;
    };
    surfaceDisposer = disposeSurface;
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

  function connect(): void {
    if (sessionDisposed) {
      throw new Error("The session has been disposed");
    }
    if (sessionConnected) return;
    if (!display || !context) {
      throw new Error("Attach a canvas before connecting the session");
    }
    if (!("VideoDecoder" in window)) {
      const error = new Error(
        "This browser does not provide the WebCodecs VideoDecoder API",
      );
      owner.updateState("video", { state: "error", message: error.message });
      emit("error", error);
      return;
    }
    sessionConnected = true;
    connectionGeneration += 1;
    resetFeedbackInterval();
    void connectControl();
    void connectVideo();
    refreshResizeObservation();
    feedbackTimer = setInterval(sendFeedback, 1000);
  }

  function disconnect(): void {
    const wasConnected = sessionConnected;
    sessionConnected = false;
    connectionGeneration += 1;
    controlConnectAttempt = null;
    videoConnectAttempt = null;
    releaseControl();
    resizeObserver?.disconnect();
    resizeObserver = null;
    window.removeEventListener("resize", scheduleResize);
    if (resizeTimer !== null) {
      clearTimeout(resizeTimer);
      resizeTimer = null;
    }
    resizePending = false;
    if (feedbackTimer !== null) {
      clearInterval(feedbackTimer);
      feedbackTimer = null;
    }
    for (const timer of [
      controlReconnectTimer,
      controlStableTimer,
      videoReconnectTimer,
    ]) {
      if (timer !== null) clearTimeout(timer);
    }
    controlReconnectTimer = null;
    controlStableTimer = null;
    videoReconnectTimer = null;
    for (const socket of [videoSocket, controlSocket]) {
      socket?.close(1000, "client disconnect");
    }
    videoSocket = null;
    controlSocket = null;
    decoderGeneration += 1;
    decoder?.close();
    decoder = null;
    resetFeedbackInterval();
    for (const frame of pendingFrames.splice(0)) frame.close();
    if (presentationTimer !== null) {
      clearTimeout(presentationTimer);
      presentationTimer = null;
    }
    if (animationFrame !== null) cancelAnimationFrame(animationFrame);
    animationFrame = null;
    animationPending = false;
    if (pointerAnimationFrame !== null)
      cancelAnimationFrame(pointerAnimationFrame);
    pointerAnimationFrame = null;
    pointerAnimationPending = false;
    pendingPointerPosition = null;
    if (wasConnected) {
      owner.updateState("video", {
        state: "disconnected",
        message: "Video disconnected",
      });
      owner.updateState("input", {
        state: "disconnected",
        message: "Input disconnected",
        connected: false,
      });
    }
  }

  function dispose(): Promise<void> {
    if (disposePromise) return disposePromise;
    sessionDisposed = true;
    disposePromise = (async () => {
      surfaceDisposer?.();
      disconnect();
      if (compositionTimer !== null) clearTimeout(compositionTimer);
      compositionTimer = null;
      suppressCompositionText = null;
      if (pendingClipboardCopy) {
        const transaction = pendingClipboardCopy;
        pendingClipboardCopy = null;
        clearTimeout(transaction.timeout);
        transaction.reject(new Error("The session has been disposed"));
      }
      if (shortcutClipboardCopy) clearTimeout(shortcutClipboardCopy.timeout);
      shortcutClipboardCopy = null;
      clipboardPastePending = false;
      pings.clear();
      resizeRequests.clear();
    })();
    return disposePromise;
  }

  return {
    attachSurface,
    connect,
    disconnect,
    dispose,
    getRemoteClipboard: () => remoteClipboard,
    remoteDisplayPolicyChanged,
    requestControl,
    releaseControl,
    sendClipboardText,
    setLatencyTarget,
  };
}

function positiveNumber(value: unknown, name: string): number {
  const number = Number(value);
  if (!Number.isFinite(number) || number <= 0) {
    throw new RangeError(`${name} must be a positive number`);
  }
  return number;
}

function optionalPositiveNumber(
  value: unknown,
  name: string,
): number | undefined {
  return value === undefined ? undefined : positiveNumber(value, name);
}

function normalizeRemoteDisplayPolicy(policy: unknown): RemoteDisplayPolicy {
  if (typeof policy !== "object" || policy === null || !("mode" in policy)) {
    throw new TypeError("Remote display policy must be an object");
  }
  if (policy.mode === "manual") return Object.freeze({ mode: "manual" });
  if (
    policy.mode === "fixed" &&
    "width" in policy &&
    "height" in policy &&
    "scale" in policy
  ) {
    return Object.freeze({
      mode: "fixed",
      width: positiveNumber(policy.width, "Remote display width"),
      height: positiveNumber(policy.height, "Remote display height"),
      scale: positiveNumber(policy.scale, "Remote display scale"),
    });
  }
  if (policy.mode !== "observe") {
    throw new TypeError(`Unknown remote display mode: ${String(policy.mode)}`);
  }
  const element = "element" in policy ? policy.element : undefined;
  if (element !== undefined && !(element instanceof Element)) {
    throw new TypeError(
      "Observed remote display element must be a DOM element",
    );
  }
  const debounceValue = "debounceMs" in policy ? policy.debounceMs : undefined;
  const debounceMs =
    debounceValue === undefined ? undefined : Number(debounceValue);
  if (
    debounceMs !== undefined &&
    (!Number.isFinite(debounceMs) || debounceMs < 0)
  ) {
    throw new RangeError("debounceMs must be a non-negative number");
  }
  const normalized: RemoteDisplayObservePolicy = {
    mode: "observe",
    ...(element === undefined ? {} : { element }),
    ...(debounceMs === undefined ? {} : { debounceMs }),
    ...optionalPolicyNumber(policy, "devicePixelRatio"),
    ...optionalPolicyNumber(policy, "minWidth"),
    ...optionalPolicyNumber(policy, "minHeight"),
    ...optionalPolicyNumber(policy, "maxWidth"),
    ...optionalPolicyNumber(policy, "maxHeight"),
    ...optionalPolicyNumber(policy, "maxPixels"),
  };
  return Object.freeze(normalized);
}

function optionalPolicyNumber<K extends keyof RemoteDisplayObservePolicy>(
  policy: object,
  name: K,
): Partial<Pick<RemoteDisplayObservePolicy, K>> {
  if (!(name in policy)) return {};
  const value = optionalPositiveNumber(Reflect.get(policy, name), String(name));
  return value === undefined
    ? {}
    : ({ [name]: value } as Partial<Pick<RemoteDisplayObservePolicy, K>>);
}

/** Framework-free browser client for a Waymote streaming gateway. */
export class WaymoteSession {
  #listeners = new Map<keyof WaymoteEventMap, Set<(value: unknown) => void>>();
  #remoteDisplayPolicy: RemoteDisplayPolicy;
  #controlOnFocus = false;
  #runtime: ReturnType<typeof createRuntime>;
  #disposed = false;
  #disposePromise: Promise<void> | null = null;

  readonly video: VideoController;
  readonly input: InputController;
  readonly clipboard: ClipboardController;
  readonly remoteDisplay: RemoteDisplayController;
  state: WaymoteSessionState;
  stats: Readonly<Partial<WaymoteStats>>;

  constructor(options: WaymoteSessionOptions = {}) {
    this.#remoteDisplayPolicy = normalizeRemoteDisplayPolicy(
      options.remoteDisplay ?? { mode: "manual" },
    );
    this.state = Object.freeze({
      video: Object.freeze({
        state: "idle",
        message: "Video idle",
        codec: null,
      }),
      input: Object.freeze({
        state: "idle",
        message: "Input idle",
        connected: false,
        pointerLocked: false,
      }),
    });
    this.stats = Object.freeze({});
    this.#runtime = createRuntime(
      {
        emit: (type, value) => this.#emit(type, value),
        updateState: (section, changes) => this.#updateState(section, changes),
        setStats: (stats) => {
          const snapshot = Object.freeze(stats);
          this.stats = snapshot;
          this.#emit("stats", snapshot);
        },
        remoteDisplayPolicy: () => this.#remoteDisplayPolicy,
        controlOnFocus: () => this.#controlOnFocus,
        setControlOnFocus: (enabled) => {
          this.#controlOnFocus = enabled;
        },
        setRemoteDisplayPolicy: (policy) =>
          this.#setRemoteDisplayPolicy(policy),
      },
      options,
    );

    const session = this;
    this.video = Object.freeze({
      setLatencyTarget: (milliseconds: number) => {
        this.#assertActive();
        this.#runtime.setLatencyTarget(milliseconds);
      },
    });
    this.input = Object.freeze({
      acquire: () => {
        this.#assertActive();
        this.#runtime.requestControl();
      },
      release: () => {
        this.#assertActive();
        this.#runtime.releaseControl();
      },
    });
    this.clipboard = Object.freeze({
      sendText: (text: string) => {
        this.#assertActive();
        return this.#runtime.sendClipboardText(String(text));
      },
      get latestRemoteText() {
        return session.#runtime.getRemoteClipboard();
      },
    });
    this.remoteDisplay = Object.freeze({
      setPolicy: (policy: RemoteDisplayPolicy) =>
        this.#setRemoteDisplayPolicy(policy),
      manual: () => this.#setRemoteDisplayPolicy({ mode: "manual" }),
      fixed: (configuration: Omit<RemoteDisplayFixedPolicy, "mode">) =>
        this.#setRemoteDisplayPolicy({ mode: "fixed", ...configuration }),
      observe: (configuration = {}) =>
        this.#setRemoteDisplayPolicy({ mode: "observe", ...configuration }),
      get policy() {
        return session.#remoteDisplayPolicy;
      },
    });
  }

  attachSurface(options: SurfaceOptions): SurfaceHandle {
    this.#assertActive();
    return this.#runtime.attachSurface(options);
  }

  on<K extends keyof WaymoteEventMap>(
    type: K,
    listener: (event: WaymoteEventMap[K]) => void,
  ): () => void {
    this.#assertActive();
    const wrapped = (value: unknown): void =>
      listener(value as WaymoteEventMap[K]);
    let listeners = this.#listeners.get(type);
    if (!listeners) this.#listeners.set(type, (listeners = new Set()));
    listeners.add(wrapped);
    return () => {
      listeners.delete(wrapped);
    };
  }

  connect(): void {
    this.#assertActive();
    this.#runtime.connect();
  }

  disconnect(): void {
    if (this.#disposed) return;
    this.#runtime.disconnect();
  }

  dispose(): Promise<void> {
    if (this.#disposePromise) return this.#disposePromise;
    this.#disposed = true;
    this.#listeners.clear();
    this.#disposePromise = this.#runtime.dispose();
    return this.#disposePromise;
  }

  #setRemoteDisplayPolicy(policy: RemoteDisplayPolicy): void {
    this.#assertActive();
    this.#remoteDisplayPolicy = normalizeRemoteDisplayPolicy(policy);
    this.#runtime?.remoteDisplayPolicyChanged();
  }

  #assertActive(): void {
    if (this.#disposed) throw new Error("The session has been disposed");
  }

  #updateState<K extends keyof WaymoteSessionState>(
    section: K,
    changes: Partial<WaymoteSessionState[K]>,
  ): void {
    const nextSection = Object.freeze({ ...this.state[section], ...changes });
    this.state = Object.freeze({ ...this.state, [section]: nextSection });
    this.#emit("state", this.state);
  }

  #emit<K extends keyof WaymoteEventMap>(
    type: K,
    value: WaymoteEventMap[K],
  ): void {
    for (const listener of [...(this.#listeners.get(type) ?? [])]) {
      try {
        listener(value);
      } catch (error) {
        console.error(`Waymote ${type} listener failed`, error);
      }
    }
  }
}

export const __testing = Object.freeze({
  fitObservedResize,
  normalizeResizeDimensions,
  LocalCursor,
});
