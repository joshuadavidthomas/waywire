// Count actual WebCodecs output frames before the viewer draws them.
const NativeVideoDecoder = window.VideoDecoder;
window.benchmarkFrames = 0;
window.benchmarkNonblank = false;
window.benchmarkFirstFrame = null;
window.benchmarkSockets = {};
window.benchmarkQuality = null;
window.benchmarkLatencies = [];
window.benchmarkArmed = false;
window.benchmarkDraws = 0;
// Decode wall time and capture timestamp (host CLOCK_MONOTONIC, microseconds).
window.benchmarkDecodes = [];
// Per-stage browser timestamps for waywire's viewer, keyed by the frame's
// capture timestamp in microseconds (the WebCodecs chunk timestamp), plus
// control-socket ping/pong pairs that measure the browser-server clock offset.
window.benchmarkTraces = [];
window.benchmarkClock = [];
const frameTimes = new Map();
const pingsSent = new Map();
const frameTime = (timestamp) => {
  let entry = frameTimes.get(timestamp);
  if (!entry) {
    entry = {};
    frameTimes.set(timestamp, entry);
    if (frameTimes.size > 600)
      frameTimes.delete(frameTimes.keys().next().value);
  }
  return entry;
};
let pendingInput = null;
document.addEventListener(
  "keydown",
  () => {
    if (!window.benchmarkArmed) return;
    const canvas = document.querySelector("#display");
    const context = canvas.getContext("2d", { willReadFrequently: true });
    const time = performance.now();
    pendingInput = {
      time,
      pixels: context.getImageData(0, 0, canvas.width, canvas.height).data,
      width: canvas.width,
      height: canvas.height,
      sent: null,
      input: null,
      // The baseline readback runs inside the keydown listener, before the
      // viewer's own handler sends the key.
      armed: performance.now() - time,
    };
    window.benchmarkArmed = false;
  },
  true,
);
const drawImage = CanvasRenderingContext2D.prototype.drawImage;
CanvasRenderingContext2D.prototype.drawImage = function (...args) {
  drawImage.apply(this, args);
  if (this.canvas.id === "display") window.benchmarkDraws++;
  const drawn = performance.now();
  if (!pendingInput || this.canvas.id !== "display") return;
  if (
    this.canvas.width !== pendingInput.width ||
    this.canvas.height !== pendingInput.height
  )
    return;
  const pixels = this.getImageData(
    0,
    0,
    this.canvas.width,
    this.canvas.height,
  ).data;
  let changed = 0;
  for (let i = 0; i < pixels.length; i += 4) {
    if (Math.abs(pixels[i] - pendingInput.pixels[i]) > 20 && ++changed >= 10) {
      const detected = performance.now();
      window.benchmarkLatencies.push(detected - pendingInput.time);
      const source = args[0];
      const timestamp =
        typeof VideoFrame !== "undefined" && source instanceof VideoFrame
          ? source.timestamp
          : null;
      window.benchmarkTraces.push({
        keydown: pendingInput.time,
        armed: pendingInput.armed,
        sent: pendingInput.sent,
        input: pendingInput.input,
        captureMicros: timestamp,
        ...(timestamp === null ? {} : frameTimes.get(timestamp)),
        drawn,
        detected,
      });
      pendingInput = null;
      break;
    }
  }
};
// Waywire records: version u8, kind u8, reserved u16, length u32, payload.
const recordKind = (data) =>
  data instanceof ArrayBuffer && data.byteLength >= 8
    ? new DataView(data).getUint8(1)
    : null;
const NativeWebSocket = window.WebSocket;
window.WebSocket = class extends NativeWebSocket {
  constructor(url, protocols) {
    super(url, protocols);
    const path = new URL(url).pathname;
    this.benchmarkPath = path;
    this.addEventListener("message", (event) => {
      const now = performance.now();
      if (path === "/control" && typeof event.data === "string") {
        try {
          const message = JSON.parse(event.data);
          if (message.type === "quality") window.benchmarkQuality = message;
          if (message.type === "pong" && pingsSent.has(message.id)) {
            window.benchmarkClock.push({
              sent: pingsSent.get(message.id),
              received: now,
              serverNanos: message.serverNanos,
            });
            pingsSent.delete(message.id);
          }
        } catch {
          // Other control messages are not JSON.
        }
      }
      // A video record (1 frame, 2 striped frame): frame kind u8, continuity u8,
      // generation u32, width u16, height u16, capture nanoseconds u64, sequence u64.
      if (path === "/stream" && [1, 2].includes(recordKind(event.data))) {
        const view = new DataView(event.data);
        if (view.byteLength >= 34) {
          const entry = frameTime(Number(view.getBigUint64(18, true) / 1000n));
          entry.received = now;
          entry.sequence = Number(view.getBigUint64(26, true));
        }
      }
      const bytes = event.data.byteLength ?? event.data.size ?? 0;
      window.benchmarkSockets[path] =
        (window.benchmarkSockets[path] ?? 0) + bytes;
    });
  }

  send(data) {
    const now = performance.now();
    if (this.benchmarkPath === "/control") {
      if (typeof data === "string") {
        try {
          const message = JSON.parse(data);
          if (message.type === "ping") pingsSent.set(message.id, now);
        } catch {
          // Only JSON pings matter here.
        }
      } else if (
        pendingInput &&
        pendingInput.sent === null &&
        recordKind(data) === 4
      ) {
        // A keyboard record: key u32, state u8, input sequence u32.
        const view = new DataView(data);
        if (view.getUint8(12) === 1) {
          pendingInput.sent = now;
          pendingInput.input = view.getUint32(13, true);
        }
      }
    }
    return super.send(data);
  }
};
window.VideoDecoder = class extends NativeVideoDecoder {
  decode(chunk) {
    // A striped frame submits one chunk per stripe; the first starts decoding.
    frameTime(chunk.timestamp).decoding ??= performance.now();
    return super.decode(chunk);
  }

  constructor(init) {
    super({
      ...init,
      output(frame) {
        frameTime(frame.timestamp).decoded = performance.now();
        window.benchmarkFrames++;
        window.benchmarkDecodes.push([Date.now(), frame.timestamp]);
        window.benchmarkFirstFrame ??= Date.now();
        try {
          if (!window.benchmarkNonblank) {
            const canvas = document.createElement("canvas");
            canvas.width = 64;
            canvas.height = 36;
            const context = canvas.getContext("2d", {
              willReadFrequently: true,
            });
            context.drawImage(frame, 0, 0, canvas.width, canvas.height);
            const pixels = context.getImageData(
              0,
              0,
              canvas.width,
              canvas.height,
            ).data;
            window.benchmarkNonblank = pixels.some(
              (value, index) => index % 4 !== 3 && value !== pixels[index % 4],
            );
          }
        } catch (error) {
          window.benchmarkError = String(error);
        }
        init.output(frame);
      },
    });
  }
};
