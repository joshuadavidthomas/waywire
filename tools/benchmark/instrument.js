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
let pendingInput = null;
document.addEventListener(
  "keydown",
  () => {
    if (!window.benchmarkArmed) return;
    const canvas = document.querySelector("#display");
    const context = canvas.getContext("2d", { willReadFrequently: true });
    pendingInput = {
      time: performance.now(),
      pixels: context.getImageData(0, 0, canvas.width, canvas.height).data,
      width: canvas.width,
      height: canvas.height,
    };
    window.benchmarkArmed = false;
  },
  true,
);
const drawImage = CanvasRenderingContext2D.prototype.drawImage;
CanvasRenderingContext2D.prototype.drawImage = function (...args) {
  drawImage.apply(this, args);
  if (this.canvas.id === "display") window.benchmarkDraws++;
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
      window.benchmarkLatencies.push(performance.now() - pendingInput.time);
      pendingInput = null;
      break;
    }
  }
};
const NativeWebSocket = window.WebSocket;
window.WebSocket = class extends NativeWebSocket {
  constructor(url, protocols) {
    super(url, protocols);
    const path = new URL(url).pathname;
    this.addEventListener("message", (event) => {
      if (path === "/control" && typeof event.data === "string") {
        try {
          const message = JSON.parse(event.data);
          if (message.type === "quality") window.benchmarkQuality = message;
        } catch {
          // Other control messages are not JSON.
        }
      }
      const bytes = event.data.byteLength ?? event.data.size ?? 0;
      window.benchmarkSockets[path] =
        (window.benchmarkSockets[path] ?? 0) + bytes;
    });
  }
};
window.VideoDecoder = class extends NativeVideoDecoder {
  constructor(init) {
    super({
      ...init,
      output(frame) {
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
