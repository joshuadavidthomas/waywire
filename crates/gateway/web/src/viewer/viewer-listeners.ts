import type {
  SurfaceHandle,
  WaywireSession,
  WaywireStats,
} from "../sdk/waywire.ts";

export type ViewerElements = {
  readonly display: HTMLCanvasElement;
  readonly empty: HTMLElement;
  readonly status: HTMLElement;
  readonly controlStatus: HTMLElement;
  readonly codec: HTMLElement;
  readonly metrics: HTMLElement;
  readonly readout: HTMLElement;
  readonly readoutToggle: HTMLButtonElement;
  readonly latency: HTMLSelectElement;
  readonly resetVideoButton: HTMLButtonElement;
  readonly pointerLockButton: HTMLButtonElement;
  readonly textInputButton: HTMLButtonElement;
  readonly sendClipboardButton: HTMLButtonElement;
  readonly copyClipboardButton: HTMLButtonElement;
  readonly clipboardStatus: HTMLElement;
  readonly imeProxy: HTMLInputElement;
};

type Slot = "size" | "fps" | "bitrate" | "rtt";
type Field =
  | "bitrate"
  | "clock"
  | "target"
  | "late"
  | "input"
  | "decode"
  | "dropped"
  | "generation"
  | "resize";

function child(
  root: HTMLElement,
  attribute: string,
  name: string,
): HTMLElement {
  const element = root.querySelector(`[${attribute}="${name}"]`);
  if (!(element instanceof HTMLElement)) {
    throw new Error(`Expected [${attribute}="${name}"] under #${root.id}`);
  }
  return element;
}

export function installViewerListeners(
  elements: ViewerElements,
  session: WaywireSession,
  surface: SurfaceHandle,
): () => void {
  const slot = (name: Slot): HTMLElement =>
    child(elements.metrics, "data-slot", name);
  const field = (name: Field): HTMLElement =>
    child(elements.readout, "data-field", name);
  const slots = {
    size: slot("size"),
    fps: slot("fps"),
    bitrate: slot("bitrate"),
    rtt: slot("rtt"),
  };
  const fields = {
    bitrate: field("bitrate"),
    clock: field("clock"),
    target: field("target"),
    late: field("late"),
    input: field("input"),
    decode: field("decode"),
    dropped: field("dropped"),
    generation: field("generation"),
    resize: field("resize"),
  };
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

  cleanup.push(
    session.on("state", (state) => {
      elements.status.textContent = state.video.message;
      elements.status.title = state.video.message;
      elements.status.dataset["state"] = state.video.state;
      elements.controlStatus.textContent = state.input.message;
      elements.controlStatus.title = state.input.message;
      elements.controlStatus.dataset["state"] = state.input.state;
      elements.codec.textContent = state.video.codec
        ? `${state.video.codec} · WebCodecs`
        : "—";
      elements.pointerLockButton.textContent = state.input.pointerLocked
        ? "Unlock pointer"
        : "Lock pointer";
      elements.pointerLockButton.setAttribute(
        "aria-pressed",
        String(state.input.pointerLocked),
      );
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
    slots.size.textContent = `${stats.width}×${stats.height}`;
    slots.fps.textContent = `${stats.renderedFps.toFixed(0)} fps`;
    slots.bitrate.textContent = `${stats.bitrateKbps} kbps · ${stats.scalePercent}%`;
    slots.rtt.textContent = `${stats.rttMs.toFixed(1)} ms rtt`;
    fields.bitrate.textContent = `${stats.bitrateKbps} kbps · ${stats.scalePercent}%`;
    fields.clock.textContent = stats.clockConfident
      ? `±${stats.clockUncertaintyMs?.toFixed(1) ?? "?"} ms`
      : "syncing";
    fields.target.textContent = `${stats.latencyTargetMs} ms`;
    fields.late.textContent = `${stats.latenessMs.toFixed(1)} ms`;
    fields.input.textContent = String(stats.pendingInputCount);
    fields.decode.textContent = String(stats.decoderQueue);
    fields.dropped.textContent = String(stats.droppedFrames);
    fields.generation.textContent = String(stats.generation);
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
      elements.clipboardStatus.title = event.status;
      elements.copyClipboardButton.disabled = event.text === null;
    }),
  );
  cleanup.push(
    session.on("resize", (event) => {
      const size =
        event.width && event.height ? ` ${event.width}×${event.height}` : "";
      const latency =
        event.latencyMs === undefined
          ? ""
          : ` in ${event.latencyMs.toFixed(0)} ms`;
      slots.size.dataset["resize"] = event.state;
      fields.resize.textContent = `${event.state}${size}${latency}`;
    }),
  );
  cleanup.push(
    session.on("error", (error) => console.warn("Waywire stream error", error)),
  );

  listen(elements.latency, "change", () =>
    session.video.setLatencyTarget(Number(elements.latency.value)),
  );
  listen(elements.resetVideoButton, "click", () => session.video.reset());
  listen(elements.textInputButton, "click", () => {
    session.input.acquire();
    surface.focusTextInput();
  });
  // The text-input key stays lit while the hidden proxy field holds focus.
  const textInputPressed = (pressed: boolean): void =>
    elements.textInputButton.setAttribute("aria-pressed", String(pressed));
  listen(elements.imeProxy, "focus", () => textInputPressed(true));
  listen(elements.imeProxy, "blur", () => textInputPressed(false));
  listen(elements.readoutToggle, "click", () => {
    const open = elements.readout.hidden;
    elements.readout.hidden = !open;
    elements.readoutToggle.setAttribute("aria-expanded", String(open));
  });
  // Keys never take focus from the screen; the lease follows the canvas.
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
