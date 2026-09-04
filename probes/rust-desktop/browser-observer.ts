// Trial-only instrumentation for the known sprite-desktop performance viewer.
// Other sessions and WebCodecs users are unsupported. Retain timing and
// video-header metadata only: no encoded payload, input, clipboard text, URLs,
// or credentials.
(() => {
  interface Header {
    atMs: number;
    sequence: string;
    generation: number;
    mediaTimestampMicros: string;
    captureNanos: string;
    flags: number;
    auBytes: number;
  }
  interface MediaTiming {
    atMs: number;
    mediaTimestampMicros: string;
  }
  interface DecodeSubmission extends MediaTiming {
    auBytes: number;
  }
  interface DecoderReset {
    atMs: number;
    queued: number;
  }
  interface LongTask {
    atMs: number;
    durationMs: number;
  }
  interface Overflow {
    headers: number;
    decodeSubmissions: number;
    decoderOutputs: number;
    resets: number;
    longTasks: number;
  }
  interface Observation {
    schema: "sprite-desktop-browser-timings-v2";
    recordingPerformanceStartMs: number;
    headers: Header[];
    decodeSubmissions: DecodeSubmission[];
    decoderOutputs: MediaTiming[];
    resets: DecoderReset[];
    longTasks: LongTask[];
    overflow: Overflow;
  }
  interface RecoveryHeader {
    atMs: number;
    sequence: string;
    generation: number;
    mediaTimestampMicros: string;
    flags: number;
  }
  interface RecoveryPoint {
    atMs: number;
    mediaTimestampMicros: string;
  }
  interface RecoveryObservation {
    schema: "sprite-desktop-decoder-recovery-v1";
    scope: string;
    injectionLabel: "injected queue-pressure observation (not actual decoder overload)";
    injectedDecodeQueueSize: 24;
    timeoutMs: 10_000;
    maxAttempts: 1;
    attemptsStarted: 1;
    finished: boolean;
    timedOut: boolean;
    drawHookInstalled: boolean;
    triggerKeyframe: RecoveryHeader | null;
    injectionDelta: RecoveryHeader | null;
    injection: {
      atMs: number;
      actualDecodeQueueSize: number;
    } | null;
    resetCount: number;
    unexpectedResetsBeforeInjection: number;
    reset: {
      atMs: number;
      actualDecodeQueueSize: number;
    } | null;
    nextKeyframe: RecoveryHeader | null;
    firstDecoderOutput: RecoveryPoint | null;
    firstDraw: RecoveryPoint | null;
  }
  interface FrameCounterConfiguration {
    bits: 16;
    cellSize: 8;
    cellCount: 36;
    minimumContrast: number;
    minimumMargin: number;
  }
  interface FrameCounterRecord {
    atMs: number;
    mediaTimestampMicros: number;
    frameId: number;
    status: number;
  }
  interface FrameCounterObservation {
    schema: "sprite-desktop-frame-counter-v1";
    drawHookInstalled: boolean;
    records: FrameCounterRecord[];
    overflow: number;
  }
  interface Observer {
    configureFrameCounter(configuration: FrameCounterConfiguration): void;
    start(): void;
    finish(): Observation;
    finishFrameCounter(): FrameCounterObservation;
    startRecovery(): RecoveryObservation;
    recoveryStatus(): RecoveryObservation;
    finishRecovery(): RecoveryObservation;
  }

  const scope = window as Window & { __rustVideoObserver?: Observer };
  if (scope.__rustVideoObserver) return;

  const limits = {
    headers: 10_000,
    decodeSubmissions: 10_000,
    decoderOutputs: 10_000,
    resets: 1_000,
    longTasks: 1_000,
  } as const;
  let startedAt = Infinity;
  let recordingFinished = false;
  let headers: Header[] = [];
  let decodeSubmissions: DecodeSubmission[] = [];
  let decoderOutputs: MediaTiming[] = [];
  let resets: DecoderReset[] = [];
  let longTasks: LongTask[] = [];
  let overflow: Overflow = emptyOverflow();

  const recoveryTimeoutMs = 10_000 as const;
  const injectedDecodeQueueSize = 24 as const;
  const injectionLabel =
    "injected queue-pressure observation (not actual decoder overload)" as const;
  let recoveryStartedAt = Infinity;
  let recoveryWasStarted = false;
  let recoveryActive = false;
  let recoveryFinished = false;
  let recoveryTimedOut = false;
  let triggerKeyframe: RecoveryHeader | null = null;
  let injectionDelta: RecoveryHeader | null = null;
  let injectionPending = false;
  let injection: RecoveryObservation["injection"] = null;
  let recoveryResetCount = 0;
  let unexpectedResetsBeforeInjection = 0;
  let recoveryReset: RecoveryObservation["reset"] = null;
  let nextKeyframe: RecoveryHeader | null = null;
  let firstDecoderOutput: RecoveryPoint | null = null;
  let firstDraw: RecoveryPoint | null = null;
  let queueHookInstalled = false;
  let drawHookInstalled = false;

  const frameCounterStatuses = {
    valid: 0,
    invalidGuards: 1,
    invalidComplement: 2,
    missingTimestamp: 3,
    unreadableRoi: 4,
  } as const;
  const frameCounterRecordLimit = 10_000;
  let frameCounterConfiguration: FrameCounterConfiguration | null = null;
  let frameCounterContext: CanvasRenderingContext2D | null = null;
  let frameCounterRecords: FrameCounterRecord[] = [];
  let frameCounterOverflow = 0;
  let frameCounterActive = false;
  let frameCounterWasStarted = false;

  function recoveryElapsed(): number {
    return performance.now() - recoveryStartedAt;
  }

  function expireRecovery(): void {
    if (recoveryActive && recoveryElapsed() >= recoveryTimeoutMs) {
      recoveryActive = false;
      recoveryTimedOut = true;
      injectionPending = false;
    }
  }

  function recoverySnapshot(): RecoveryObservation {
    expireRecovery();
    if (!recoveryWasStarted) {
      throw new Error("Recovery observation was not started");
    }
    return {
      schema: "sprite-desktop-decoder-recovery-v1",
      scope:
        "One probe-only decodeQueueSize read is replaced with 24. The unchanged SDK makes the queue-cap decision and calls the real decoder reset; reset, output, and draw fields report actual browser operations.",
      injectionLabel,
      injectedDecodeQueueSize,
      timeoutMs: recoveryTimeoutMs,
      maxAttempts: 1,
      attemptsStarted: 1,
      finished: recoveryFinished,
      timedOut: recoveryTimedOut,
      drawHookInstalled,
      triggerKeyframe,
      injectionDelta,
      injection,
      resetCount: recoveryResetCount,
      unexpectedResetsBeforeInjection,
      reset: recoveryReset,
      nextKeyframe,
      firstDecoderOutput,
      firstDraw,
    };
  }

  function recoveryHeader(view: DataView): RecoveryHeader {
    return {
      atMs: recoveryElapsed(),
      sequence: view.getBigUint64(4, true).toString(),
      mediaTimestampMicros: view.getBigUint64(12, true).toString(),
      generation: view.getUint32(20, true),
      flags: view.getUint8(2),
    };
  }

  function observeRecoveryHeader(view: DataView): void {
    expireRecovery();
    if (!recoveryActive) return;
    const header = recoveryHeader(view);
    const keyframe = (header.flags & 1) !== 0;
    if (!injection) {
      if (!triggerKeyframe) {
        if ((header.flags & 3) === 1) triggerKeyframe = header;
        return;
      }
      if (!injectionDelta && (header.flags & 3) === 0) {
        injectionDelta = header;
        injectionPending = true;
      }
      return;
    }
    if (recoveryReset && !nextKeyframe && keyframe) nextKeyframe = header;
  }

  function injectedQueueObservation(actualDecodeQueueSize: number): number {
    expireRecovery();
    if (!recoveryActive || !injectionPending || injection) {
      return actualDecodeQueueSize;
    }
    injectionPending = false;
    injection = {
      atMs: recoveryElapsed(),
      actualDecodeQueueSize,
    };
    return injectedDecodeQueueSize;
  }

  function stopCompletedRecovery(): void {
    if (
      recoveryActive &&
      injection &&
      recoveryReset &&
      nextKeyframe &&
      firstDecoderOutput &&
      (!drawHookInstalled || firstDraw)
    ) {
      recoveryActive = false;
      injectionPending = false;
    }
  }

  function emptyOverflow(): Overflow {
    return {
      headers: 0,
      decodeSubmissions: 0,
      decoderOutputs: 0,
      resets: 0,
      longTasks: 0,
    };
  }

  function elapsed(): number {
    return performance.now() - startedAt;
  }

  function inWindow(atMs: number): boolean {
    return atMs >= 0 && atMs < 30_000;
  }

  function append<K extends keyof Overflow, T>(
    name: K,
    values: T[],
    value: T,
  ): void {
    if (values.length < limits[name]) values.push(value);
    else overflow[name] += 1;
  }

  const collectLongTasks = (entries: readonly PerformanceEntry[]) => {
    for (const entry of entries) {
      const atMs = entry.startTime - startedAt;
      if (inWindow(atMs)) {
        append("longTasks", longTasks, { atMs, durationMs: entry.duration });
      }
    }
  };
  let longTaskObserver: PerformanceObserver | undefined;
  if (
    typeof PerformanceObserver !== "undefined" &&
    PerformanceObserver.supportedEntryTypes.includes("longtask")
  ) {
    longTaskObserver = new PerformanceObserver((list) => {
      collectLongTasks(list.getEntries());
    });
    longTaskObserver.observe({ type: "longtask" });
  }

  if (typeof VideoDecoder !== "undefined") {
    const NativeVideoDecoder = VideoDecoder;
    const nativeReset = NativeVideoDecoder.prototype.reset;
    const nativeDecode = NativeVideoDecoder.prototype.decode;
    const nativeQueueGetter = Object.getOwnPropertyDescriptor(
      NativeVideoDecoder.prototype,
      "decodeQueueSize",
    )?.get;
    NativeVideoDecoder.prototype.reset = function () {
      const actualDecodeQueueSize = this.decodeQueueSize;
      const atMs = elapsed();
      if (inWindow(atMs)) {
        append("resets", resets, { atMs, queued: actualDecodeQueueSize });
      }
      expireRecovery();
      if (recoveryActive) {
        recoveryResetCount += 1;
        if (!injection) unexpectedResetsBeforeInjection += 1;
        else if (!recoveryReset) {
          recoveryReset = {
            atMs: recoveryElapsed(),
            actualDecodeQueueSize,
          };
        }
      }
      return nativeReset.call(this);
    };
    NativeVideoDecoder.prototype.decode = function (chunk) {
      const atMs = elapsed();
      if (inWindow(atMs)) {
        append("decodeSubmissions", decodeSubmissions, {
          atMs,
          mediaTimestampMicros: String(chunk.timestamp),
          auBytes: chunk.byteLength,
        });
      }
      return nativeDecode.call(this, chunk);
    };
    const InstrumentedVideoDecoder = new Proxy(NativeVideoDecoder, {
      construct(target, args, newTarget) {
        const init = args[0] as VideoDecoderInit;
        const output = init.output;
        const wrapped: VideoDecoderInit = {
          ...init,
          output(frame) {
            const atMs = elapsed();
            if (inWindow(atMs)) {
              append("decoderOutputs", decoderOutputs, {
                atMs,
                mediaTimestampMicros: String(frame.timestamp),
              });
            }
            expireRecovery();
            if (recoveryActive && recoveryReset && !firstDecoderOutput) {
              firstDecoderOutput = {
                atMs: recoveryElapsed(),
                mediaTimestampMicros: String(frame.timestamp),
              };
              stopCompletedRecovery();
            }
            output(frame);
          },
        };
        const decoder = Reflect.construct(target, [wrapped], newTarget);
        if (nativeQueueGetter) {
          Object.defineProperty(decoder, "decodeQueueSize", {
            configurable: true,
            get() {
              const actualDecodeQueueSize = nativeQueueGetter.call(decoder) as
                | number
                | undefined;
              if (actualDecodeQueueSize === undefined) return 0;
              return injectedQueueObservation(actualDecodeQueueSize);
            },
          });
          queueHookInstalled = true;
        }
        return decoder;
      },
    });
    window.VideoDecoder = InstrumentedVideoDecoder;
  }

  if (typeof CanvasRenderingContext2D !== "undefined") {
    const prototype = CanvasRenderingContext2D.prototype;
    const descriptor = Object.getOwnPropertyDescriptor(prototype, "drawImage");
    const nativeDrawImage = descriptor?.value;
    if (typeof nativeDrawImage === "function") {
      Object.defineProperty(prototype, "drawImage", {
        ...descriptor,
        value: function (this: CanvasRenderingContext2D, ...args: unknown[]) {
          const result = Reflect.apply(nativeDrawImage, this, args);
          expireRecovery();
          const source = args[0];
          if (
            recoveryActive &&
            recoveryReset &&
            !firstDraw &&
            source !== null &&
            typeof source === "object" &&
            "timestamp" in source &&
            typeof source.timestamp === "number"
          ) {
            firstDraw = {
              atMs: recoveryElapsed(),
              mediaTimestampMicros: String(source.timestamp),
            };
            stopCompletedRecovery();
          }
          // The local recorder installs an own drawImage wrapper only for its
          // active recording. Follow that exact lifetime, including a delayed
          // final timer, rather than a second independent 30-second cutoff.
          if (
            frameCounterActive &&
            this.canvas.id === "display" &&
            Object.hasOwn(this, "drawImage")
          ) {
            const atMs = elapsed();
            let mediaTimestampMicros = -1;
            let frameId = -1;
            let status: number = frameCounterStatuses.missingTimestamp;
            if (
              source !== null &&
              typeof source === "object" &&
              "timestamp" in source &&
              typeof source.timestamp === "number" &&
              Number.isSafeInteger(source.timestamp) &&
              source.timestamp >= 0
            ) {
              mediaTimestampMicros = source.timestamp;
              status = frameCounterStatuses.unreadableRoi;
              const configuration = frameCounterConfiguration;
              const context = frameCounterContext;
              if (configuration && context) {
                try {
                  Reflect.apply(nativeDrawImage, context, [
                    source,
                    0,
                    0,
                    configuration.cellSize * configuration.cellCount,
                    configuration.cellSize,
                    0,
                    0,
                    configuration.cellCount,
                    1,
                  ]);
                  const pixels = context.getImageData(
                    0,
                    0,
                    configuration.cellCount,
                    1,
                  ).data;
                  const luminance = Array.from(
                    { length: configuration.cellCount },
                    (_, index) =>
                      (pixels[index * 4]! +
                        pixels[index * 4 + 1]! +
                        pixels[index * 4 + 2]!) /
                      3,
                  );
                  const white = (luminance[0]! + luminance[35]!) / 2;
                  const black = (luminance[1]! + luminance[34]!) / 2;
                  const threshold = (white + black) / 2;
                  const classify = (value: number): number | null => {
                    if (
                      Math.abs(value - threshold) < configuration.minimumMargin
                    )
                      return null;
                    return value > threshold ? 1 : 0;
                  };
                  if (
                    white - black < configuration.minimumContrast ||
                    classify(luminance[0]!) !== 1 ||
                    classify(luminance[1]!) !== 0 ||
                    classify(luminance[34]!) !== 0 ||
                    classify(luminance[35]!) !== 1
                  ) {
                    status = frameCounterStatuses.invalidGuards;
                  } else {
                    frameId = 0;
                    status = frameCounterStatuses.valid;
                    for (let bit = 0; bit < configuration.bits; bit += 1) {
                      const value = classify(luminance[2 + bit * 2]!);
                      const inverse = classify(luminance[3 + bit * 2]!);
                      if (
                        value === null ||
                        inverse === null ||
                        value === inverse
                      ) {
                        frameId = -1;
                        status = frameCounterStatuses.invalidComplement;
                        break;
                      }
                      frameId += value * 2 ** bit;
                    }
                  }
                } catch {
                  frameId = -1;
                  status = frameCounterStatuses.unreadableRoi;
                }
              }
            }
            const record = { atMs, mediaTimestampMicros, frameId, status };
            if (frameCounterRecords.length < frameCounterRecordLimit)
              frameCounterRecords.push(record);
            else frameCounterOverflow += 1;
          }
          return result;
        },
      });
      drawHookInstalled = true;
    }
  }

  const NativeWebSocket = window.WebSocket;
  const InstrumentedWebSocket = new Proxy(NativeWebSocket, {
    construct(target, args, newTarget) {
      const socket = Reflect.construct(target, args, newTarget) as WebSocket;
      const url = args[0];
      if (
        (typeof url === "string" || url instanceof URL) &&
        new URL(url, location.href).pathname === "/stream"
      ) {
        socket.addEventListener("message", (event) => {
          if (
            !(event.data instanceof ArrayBuffer) ||
            event.data.byteLength < 40
          )
            return;
          const view = new DataView(event.data);
          if (view.getUint8(0) !== 2 || view.getUint8(1) !== 1) return;
          observeRecoveryHeader(view);
          const atMs = elapsed();
          if (!inWindow(atMs)) return;
          append("headers", headers, {
            atMs,
            sequence: view.getBigUint64(4, true).toString(),
            mediaTimestampMicros: view.getBigUint64(12, true).toString(),
            generation: view.getUint32(20, true),
            captureNanos: view.getBigUint64(28, true).toString(),
            flags: view.getUint8(2),
            auBytes: event.data.byteLength - 40,
          });
        });
      }
      return socket;
    },
  });
  window.WebSocket = InstrumentedWebSocket;

  function assertRecordingStarted(value: number): void {
    if (!Number.isFinite(value) || value < 0) {
      throw new Error("Browser timing observation was not started");
    }
  }

  scope.__rustVideoObserver = {
    configureFrameCounter(configuration) {
      if (frameCounterWasStarted || frameCounterConfiguration) {
        throw new Error(
          "Frame counter can be configured only once before recording",
        );
      }
      if (
        configuration.bits !== 16 ||
        configuration.cellSize !== 8 ||
        configuration.cellCount !== 36 ||
        !Number.isFinite(configuration.minimumContrast) ||
        configuration.minimumContrast <= 0 ||
        !Number.isFinite(configuration.minimumMargin) ||
        configuration.minimumMargin <= 0
      ) {
        throw new Error("Invalid frame counter configuration");
      }
      if (!drawHookInstalled) throw new Error("Draw hook is unavailable");
      const canvas = document.createElement("canvas");
      canvas.width = configuration.cellCount;
      canvas.height = 1;
      const context = canvas.getContext("2d", { willReadFrequently: true });
      if (!context) throw new Error("Frame counter read canvas is unavailable");
      frameCounterConfiguration = configuration;
      frameCounterContext = context;
    },
    start() {
      if (recoveryWasStarted) {
        throw new Error("Recording cannot start after the recovery exercise");
      }
      headers = [];
      decodeSubmissions = [];
      decoderOutputs = [];
      resets = [];
      longTasks = [];
      overflow = emptyOverflow();
      frameCounterRecords = [];
      frameCounterOverflow = 0;
      frameCounterActive = frameCounterConfiguration !== null;
      frameCounterWasStarted = frameCounterActive;
      recordingFinished = false;
      startedAt = performance.now();
    },
    finish() {
      collectLongTasks(longTaskObserver?.takeRecords() ?? []);
      const recordingPerformanceStartMs = startedAt;
      assertRecordingStarted(recordingPerformanceStartMs);
      frameCounterActive = false;
      startedAt = Infinity;
      recordingFinished = true;
      longTaskObserver?.disconnect();
      return {
        schema: "sprite-desktop-browser-timings-v2",
        recordingPerformanceStartMs,
        headers,
        decodeSubmissions,
        decoderOutputs,
        resets,
        longTasks,
        overflow,
      };
    },
    finishFrameCounter() {
      if (!frameCounterWasStarted || frameCounterActive) {
        throw new Error("Frame counter recording has not finished");
      }
      return {
        schema: "sprite-desktop-frame-counter-v1",
        drawHookInstalled,
        records: frameCounterRecords,
        overflow: frameCounterOverflow,
      };
    },
    startRecovery() {
      if (!recordingFinished || startedAt !== Infinity) {
        throw new Error("Recovery must start after recording finishes");
      }
      if (recoveryWasStarted) {
        throw new Error("Recovery observation can run only once");
      }
      if (!queueHookInstalled) {
        throw new Error("VideoDecoder queue observation hook is unavailable");
      }
      recoveryWasStarted = true;
      recoveryActive = true;
      recoveryStartedAt = performance.now();
      return recoverySnapshot();
    },
    recoveryStatus() {
      return recoverySnapshot();
    },
    finishRecovery() {
      const snapshot = recoverySnapshot();
      recoveryActive = false;
      recoveryFinished = true;
      injectionPending = false;
      return { ...snapshot, finished: true };
    },
  };
})();
