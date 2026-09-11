import type { SurfaceHandle, WaywireSession, WaywireStats } from "./waywire.ts";

export type ViewerElements = {
  readonly display: HTMLCanvasElement;
  readonly empty: HTMLElement;
  readonly status: HTMLElement;
  readonly controlStatus: HTMLElement;
  readonly codec: HTMLElement;
  readonly metrics: HTMLElement;
  readonly latency: HTMLSelectElement;
  readonly pointerLockButton: HTMLButtonElement;
  readonly textInputButton: HTMLButtonElement;
  readonly sendClipboardButton: HTMLButtonElement;
  readonly copyClipboardButton: HTMLButtonElement;
  readonly clipboardStatus: HTMLElement;
  readonly imeProxy: HTMLInputElement;
};

export function installViewerListeners(
  elements: ViewerElements,
  session: WaywireSession,
  surface: SurfaceHandle,
): () => void {
  let resizeSummary = "resize idle";
  let pendingMetrics: WaywireStats | null = null;
  let metricsTimer: ReturnType<typeof setTimeout> | null = null;
  let lastMetricsUpdate = Number.NEGATIVE_INFINITY;
  const cleanup: Array<() => void> = [];
  const listen = (
    target: EventTarget,
    type: string,
    listener: EventListener,
  ): void => {
    target.addEventListener(type, listener);
    cleanup.push(() => target.removeEventListener(type, listener));
  };
  const setConnectionStyle = (
    element: HTMLElement,
    connected: boolean,
  ): void => {
    element.classList.toggle("connected", connected);
    element.classList.toggle("disconnected", !connected);
  };

  cleanup.push(
    session.on("state", (state) => {
      elements.status.textContent = state.video.message;
      elements.controlStatus.textContent = state.input.message;
      elements.codec.textContent = state.video.codec
        ? `${state.video.codec} · WebCodecs`
        : "H.264 · WebCodecs";
      setConnectionStyle(elements.status, state.video.state === "connected");
      setConnectionStyle(
        elements.controlStatus,
        state.input.state === "active" || state.input.state === "ready",
      );
      elements.pointerLockButton.textContent = state.input.pointerLocked
        ? "Unlock pointer"
        : "Lock pointer";
      if (state.video.state === "error") {
        elements.empty.textContent = state.video.message;
        elements.empty.classList.remove("hidden");
      }
    }),
  );
  const renderMetrics = (): void => {
    metricsTimer = null;
    const stats = pendingMetrics;
    if (!stats) return;
    pendingMetrics = null;
    lastMetricsUpdate = performance.now();
    const clock = stats.clockConfident
      ? `clock ±${stats.clockUncertaintyMs?.toFixed(1) ?? "?"} ms`
      : "clock syncing";
    elements.metrics.textContent = [
      `${stats.width}×${stats.height}`,
      `${stats.renderedFps.toFixed(0)} fps`,
      `${stats.bitrateKbps} kbps @ ${stats.scalePercent}%`,
      `${stats.rttMs.toFixed(1)} ms RTT`,
      clock,
      `target ${stats.latencyTargetMs} ms`,
      `late ${stats.latenessMs.toFixed(1)} ms`,
      `input ${stats.pendingInputCount}`,
      `decode ${stats.decoderQueue}`,
      `dropped ${stats.droppedFrames}`,
      resizeSummary,
    ].join(" · ");
  };
  cleanup.push(
    session.on("stats", (stats) => {
      elements.empty.classList.add("hidden");
      pendingMetrics = stats;
      const remaining = 250 - (performance.now() - lastMetricsUpdate);
      if (remaining <= 0) {
        if (metricsTimer !== null) clearTimeout(metricsTimer);
        renderMetrics();
      } else if (metricsTimer === null) {
        metricsTimer = setTimeout(renderMetrics, remaining);
      }
    }),
  );
  cleanup.push(
    session.on("clipboard", (event) => {
      elements.clipboardStatus.textContent = event.status;
      elements.copyClipboardButton.disabled = event.text === null;
    }),
  );
  cleanup.push(
    session.on("resize", (event) => {
      const size =
        event.width && event.height ? `${event.width}×${event.height}` : "";
      const latency =
        event.latencyMs === undefined
          ? ""
          : ` in ${event.latencyMs.toFixed(0)} ms`;
      resizeSummary = `resize ${event.state} ${size}${latency}`.trim();
    }),
  );
  cleanup.push(
    session.on("error", (error) => console.warn("Waywire stream error", error)),
  );

  listen(elements.latency, "change", () =>
    session.video.setLatencyTarget(Number(elements.latency.value)),
  );
  listen(elements.textInputButton, "click", () => {
    session.input.acquire();
    surface.focusTextInput();
  });
  for (const button of document.querySelectorAll("button")) {
    listen(button, "pointerdown", (event) => event.preventDefault());
  }
  return () => {
    if (metricsTimer !== null) clearTimeout(metricsTimer);
    metricsTimer = null;
    pendingMetrics = null;
    for (const remove of cleanup.splice(0).reverse()) remove();
  };
}
