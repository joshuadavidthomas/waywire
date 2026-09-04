import { summarize } from "./metrics.mjs";

// Opt-in diagnostic. Counts payload lengths only, never keys, clipboard, pixels,
// socket URLs, cookies or credentials. Install before opening transport sockets.
export function installRecorder({ transport, canvas }) {
  let active = null;
  let result = null;
  let started = 0;
  let restoreCanvas = () => {};
  let animation = 0;
  let timer = 0;
  let sampleTimer = 0;
  let observer = null;
  let canvasDirty = false;
  const NativeWebSocket = window.WebSocket;
  const encoder = new TextEncoder();
  const byteLength = (data) =>
    typeof data === "string"
      ? encoder.encode(data).byteLength
      : (data.size ?? data.byteLength);
  const elapsed = () => performance.now() - started;
  const warn = (text) => {
    if (active && !active.warnings.includes(text)) active.warnings.push(text);
  };

  window.WebSocket = new Proxy(NativeWebSocket, {
    construct(Target, args) {
      const socket = Reflect.construct(Target, args);
      socket.addEventListener("message", (event) => {
        if (active) active.receivedBytes += byteLength(event.data);
      });
      socket.addEventListener("close", () =>
        warn("A WebSocket closed during recording."),
      );
      const send = socket.send;
      socket.send = function (data) {
        const value = send.call(this, data);
        if (active) active.sentBytes += byteLength(data);
        return value;
      };
      return socket;
    },
  });

  const panel = document.createElement("section");
  panel.setAttribute("aria-label", "Comparison recording");
  panel.style.cssText =
    "position:fixed;z-index:10000;top:8px;left:8px;max-width:calc(100vw - 16px);padding:10px;background:#17202b;color:#fff;border:1px solid #718096;border-radius:6px;font:14px system-ui;display:flex;flex-wrap:wrap;align-items:center;gap:8px";
  const label = document.createElement("label");
  label.textContent = "Workload ";
  const workload = document.createElement("select");
  for (const name of ["desktop use", "continuous motion", "still desktop"]) {
    workload.add(new Option(name, name));
  }
  label.append(workload);
  const start = document.createElement("button");
  start.textContent = "Record 30 seconds";
  const stop = document.createElement("button");
  stop.textContent = "Stop";
  stop.disabled = true;
  const download = document.createElement("button");
  download.textContent = "Download JSON";
  download.disabled = true;
  const status = document.createElement("span");
  status.setAttribute("role", "status");
  status.textContent = `${transport}: ready`;
  for (const control of [workload, start, stop, download]) {
    control.style.cssText =
      "font:inherit;background:#fff;color:#17202b;border:1px solid #718096;border-radius:3px;padding:4px 8px";
  }
  panel.append(label, start, stop, download, status);
  document.body.append(panel);

  const dimensions = (surface) => {
    const rect = surface.getBoundingClientRect();
    return {
      width: surface.width,
      height: surface.height,
      cssWidth: rect.width,
      cssHeight: rect.height,
      devicePixelRatio: window.devicePixelRatio,
    };
  };
  const collectTasks = (entries) => {
    if (!active) return;
    for (const entry of entries) {
      if (entry.startTime >= started)
        active.longTasks.push({
          atMs: entry.startTime - started,
          durationMs: entry.duration,
        });
    }
  };
  function finish() {
    if (!active) return;
    if (observer) collectTasks(observer.takeRecords());
    observer?.disconnect();
    observer = null;
    cancelAnimationFrame(animation);
    clearTimeout(timer);
    clearInterval(sampleTimer);
    restoreCanvas();
    active.durationMs = elapsed();
    active.endedAt = new Date().toISOString();
    const surface = canvas();
    active.finalDimensions = surface ? dimensions(surface) : null;
    if (
      JSON.stringify(active.dimensions) !==
      JSON.stringify(active.finalDimensions)
    )
      warn("Canvas size, CSS size or DPR changed during recording.");
    result = { ...active, summary: summarize(active) };
    active = null;
    start.disabled = false;
    workload.disabled = false;
    stop.disabled = true;
    download.disabled = false;
    const summary = result.summary;
    status.textContent = `${summary.canvasUpdateFrames} update frames; spacing p95 ${summary.canvasUpdateFrameSpacingMs.p95?.toFixed(1) ?? "—"} ms; ${summary.receivedPayloadMbps.toFixed(2)} Mbps payload${summary.warnings.length ? "; see warnings" : ""}`;
  }
  start.addEventListener("click", () => {
    const surface = canvas();
    if (!surface || surface.width < 1 || !surface.isConnected) {
      status.textContent = "Connect the desktop before recording.";
      return;
    }
    if (document.visibilityState !== "visible") return;
    const context = surface.getContext("2d");
    if (!context) {
      status.textContent = "This recorder requires a 2D canvas.";
      return;
    }
    result = null;
    started = performance.now();
    active = {
      schema: 1,
      transport,
      workload: workload.value,
      startedAt: new Date().toISOString(),
      durationMs: 0,
      browser: navigator.userAgent,
      dimensions: dimensions(surface),
      hardwareConcurrency: navigator.hardwareConcurrency,
      canvasSubmitsMs: [],
      canvasUpdateFramesMs: [],
      animationFramesMs: [],
      longTasks: [],
      sdkStats: [],
      receivedBytes: 0,
      sentBytes: 0,
      samples: [],
      warnings: [],
      definitions: {
        canvasSubmitsMs:
          "Completed drawImage calls on the visible canvas, not physical screen presentations. Multiple calls can appear in one browser frame.",
        canvasUpdateFramesMs:
          "Browser animation callbacks following at least one canvas draw. Batches partial updates; not physical presentations. Gaps on still content are not evidence of stutter.",
        animationFramesMs:
          "Browser requestAnimationFrame callbacks, not remote video frames.",
        bandwidth:
          "Application WebSocket payload bytes across all sockets; excludes WebSocket/TLS/IP overhead. Waymote bitrateKbps is an encoder target, not measured bandwidth.",
        inputLatency:
          "Not measured. A frame after input is not proof that it contains the input response.",
        privacy:
          "No pixels, input contents, clipboard, socket URLs or credentials are recorded.",
      },
    };
    const drawImage = context.drawImage;
    const hadOwn = Object.hasOwn(context, "drawImage");
    context.drawImage = function (...args) {
      const value = drawImage.apply(this, args);
      if (active) {
        active.canvasSubmitsMs.push(elapsed());
        canvasDirty = true;
      }
      return value;
    };
    restoreCanvas = () => {
      if (hadOwn) context.drawImage = drawImage;
      else delete context.drawImage;
    };
    canvasDirty = false;
    const tick = () => {
      if (!active) return;
      const now = elapsed();
      active.animationFramesMs.push(now);
      if (canvasDirty) active.canvasUpdateFramesMs.push(now);
      canvasDirty = false;
      if (canvas() !== surface) {
        warn("The visible canvas was replaced; recording stopped.");
        finish();
        return;
      }
      animation = requestAnimationFrame(tick);
    };
    animation = requestAnimationFrame(tick);
    if (PerformanceObserver.supportedEntryTypes.includes("longtask")) {
      observer = new PerformanceObserver((list) =>
        collectTasks(list.getEntries()),
      );
      observer.observe({ type: "longtask" });
    } else warn("This browser does not expose long-task entries.");
    sampleTimer = setInterval(() => {
      if (!active) return;
      active.samples.push({
        atMs: elapsed(),
        receivedBytes: active.receivedBytes,
        sentBytes: active.sentBytes,
        canvasSubmits: active.canvasSubmitsMs.length,
      });
      status.textContent = `${Math.max(0, Math.ceil(30 - elapsed() / 1000))}s left — ${active.canvasSubmitsMs.length} updates`;
      if (
        JSON.stringify(dimensions(surface)) !==
        JSON.stringify(active.dimensions)
      )
        warn("Canvas size, CSS size or DPR changed during recording.");
    }, 1000);
    timer = setTimeout(finish, 30_000);
    start.disabled = true;
    workload.disabled = true;
    stop.disabled = false;
    download.disabled = true;
    status.textContent = "Recording — use the desktop";
  });
  stop.addEventListener("click", finish);
  document.addEventListener("visibilitychange", () => {
    if (active && document.visibilityState !== "visible") {
      warn("Tab became hidden; recording stopped.");
      finish();
    }
  });
  download.addEventListener("click", () => {
    if (!result) return;
    const url = URL.createObjectURL(
      new Blob([JSON.stringify(result, null, 2)], { type: "application/json" }),
    );
    const link = document.createElement("a");
    link.href = url;
    link.download = `${transport}-${result.startedAt.replaceAll(":", "-")}.json`;
    link.click();
    setTimeout(() => URL.revokeObjectURL(url), 10_000);
  });
  return {
    recordStats(stats) {
      if (active) active.sdkStats.push({ atMs: elapsed(), ...stats });
    },
    // Test access contains the same credential-free data as Download JSON.
    get result() {
      return result;
    },
  };
}
