// Framework-independent browser SDK for a Waymote streaming gateway.
import { ControlRuntime, type TransportPath } from "./control.ts";
import {
  normalizeRemoteDisplayPolicy,
  type RemoteDisplayFixedPolicy,
  type RemoteDisplayObservePolicy,
  type RemoteDisplayPolicy,
} from "./resize.ts";

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

/** Framework-free browser client for a Waymote streaming gateway. */
export class WaymoteSession {
  #listeners = new Map<keyof WaymoteEventMap, Set<(value: unknown) => void>>();
  #remoteDisplayPolicy: RemoteDisplayPolicy;
  #controlOnFocus = false;
  #runtime: ControlRuntime;
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
    this.#runtime = new ControlRuntime(
      {
        emit: (type, value) => this.#emit(type, value),
        updateState: (section, changes) => this.#updateState(section, changes),
        setStats: (stats) => {
          const snapshot = Object.freeze(stats);
          this.stats = snapshot;
          this.#emit("stats", snapshot);
        },
        halt: (error) => this.#halt(error),
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

  #halt(error: Error): void {
    this.#runtime.disconnect();
    this.#updateState("video", {
      state: "error",
      message: error.message,
    });
    this.#emit("error", error);
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
