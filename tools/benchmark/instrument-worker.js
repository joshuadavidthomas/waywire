// The benchmark instrument for waywire's video worker, which decodes and shows
// frames off the page: the same hooks as instrument.js, reporting to the page.
// worker_instrument.mjs runs this in the worker, and instrument.js on the page
// holds the viewer's messages to the worker until it says hello.
// Times are the worker's performance.now(); each message carries its
// timeOrigin so the page can shift them onto its own clock.
(() => {
  // Change nothing until the worker's interfaces exist, so a retry is safe.
  if (typeof VideoDecoder !== "function" || typeof WebSocket !== "function")
    throw new Error("the worker's interfaces are not ready");
  const report = (message) =>
    self.postMessage({
      benchmark: { origin: performance.timeOrigin, ...message },
    });
  const frameTimes = new Map();
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
  let display = null;
  let pendingInput = null;
  let lastShown = null;
  let nonblank = false;

  // The page holds the viewer's messages until the hello at the end, so this
  // sees the display canvas arrive whenever it runs. Arming messages carry
  // the page's last run state and no command, so the viewer ignores them.
  self.addEventListener("message", (event) => {
    if (event.data?.type === "attach") display = event.data.canvas;
    if (event.data?.benchmarkArm) arm();
  });
  const arm = () => {
    const context = display?.getContext("2d");
    const viaTrack = lastShown !== null;
    pendingInput = {
      pixels:
        viaTrack || !context
          ? null
          : context.getImageData(0, 0, display.width, display.height).data,
      frame: viaTrack ? lastShown.clone() : null,
      width: display?.width,
      height: display?.height,
    };
  };
  const detect = (input, before, after, frame, drawn) => {
    if (pendingInput !== input || before.length !== after.length) return;
    let changed = 0;
    for (let i = 0; i < after.length; i += 4) {
      if (Math.abs(after[i] - before[i]) > 20 && ++changed >= 10) {
        const timestamp = frame instanceof VideoFrame ? frame.timestamp : null;
        report({
          trace: {
            captureMicros: timestamp,
            ...(timestamp === null ? {} : frameTimes.get(timestamp)),
            drawn,
            detected: performance.now(),
          },
        });
        input.frame?.close();
        pendingInput = null;
        return;
      }
    }
  };

  const drawImage = OffscreenCanvasRenderingContext2D.prototype.drawImage;
  OffscreenCanvasRenderingContext2D.prototype.drawImage = function (...args) {
    drawImage.apply(this, args);
    if (this.canvas !== display) return;
    const drawn = performance.now();
    report({ draws: 1 });
    if (
      !pendingInput?.pixels ||
      this.canvas.width !== pendingInput.width ||
      this.canvas.height !== pendingInput.height
    )
      return;
    detect(
      pendingInput,
      pendingInput.pixels,
      this.getImageData(0, 0, this.canvas.width, this.canvas.height).data,
      args[0],
      drawn,
    );
  };
  const frameBytes = async (frame) => {
    const bytes = new Uint8Array(frame.allocationSize());
    await frame.copyTo(bytes);
    return bytes;
  };
  const write = WritableStreamDefaultWriter.prototype.write;
  WritableStreamDefaultWriter.prototype.write = function (chunk) {
    if (!(chunk instanceof VideoFrame)) return write.call(this, chunk);
    const shown = chunk.clone();
    const result = write.call(this, chunk);
    const drawn = performance.now();
    report({ draws: 1 });
    const input = pendingInput;
    if (input?.frame) {
      const baseline = input.frame;
      const frame = shown.clone();
      Promise.all([frameBytes(baseline), frameBytes(frame)])
        .then(([before, after]) => detect(input, before, after, frame, drawn))
        .catch(() => undefined)
        .finally(() => frame.close());
    }
    lastShown?.close();
    lastShown = shown;
    return result;
  };

  const recordKind = (data) =>
    data instanceof ArrayBuffer && data.byteLength >= 8
      ? new DataView(data).getUint8(1)
      : null;
  const NativeWebSocket = self.WebSocket;
  self.WebSocket = class extends NativeWebSocket {
    constructor(url, protocols) {
      super(url, protocols);
      const path = new URL(url).pathname;
      this.addEventListener("message", (event) => {
        const now = performance.now();
        if (path === "/stream" && [1, 2].includes(recordKind(event.data))) {
          const view = new DataView(event.data);
          if (view.byteLength >= 34) {
            const entry = frameTime(
              Number(view.getBigUint64(18, true) / 1000n),
            );
            entry.received = now;
            entry.sequence = Number(view.getBigUint64(26, true));
          }
        }
        report({
          socket: path,
          bytes: event.data.byteLength ?? event.data.size ?? 0,
        });
      });
    }
  };

  const NativeVideoDecoder = self.VideoDecoder;
  self.VideoDecoder = class extends NativeVideoDecoder {
    decode(chunk) {
      frameTime(chunk.timestamp).decoding ??= performance.now();
      return super.decode(chunk);
    }

    constructor(init) {
      super({
        ...init,
        output(frame) {
          frameTime(frame.timestamp).decoded = performance.now();
          if (!nonblank) {
            const canvas = new OffscreenCanvas(64, 36);
            const context = canvas.getContext("2d", {
              willReadFrequently: true,
            });
            context.drawImage(frame, 0, 0, canvas.width, canvas.height);
            const pixels = context.getImageData(0, 0, 64, 36).data;
            nonblank = pixels.some(
              (value, index) => index % 4 !== 3 && value !== pixels[index % 4],
            );
          }
          report({ decode: [Date.now(), frame.timestamp], nonblank });
          init.output(frame);
        },
      });
    }
  };
  report({ hello: true });
})();
