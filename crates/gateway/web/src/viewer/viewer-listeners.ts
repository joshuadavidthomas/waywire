import type {
  SurfaceHandle,
  WaywireSession,
  WaywireSessionState,
  WaywireStats,
} from "../sdk/waywire.ts";

export type ViewerElements = {
  readonly stage: HTMLElement;
  readonly display: HTMLCanvasElement;
  readonly signal: HTMLElement;
  readonly signalHeadline: HTMLElement;
  readonly signalMessage: HTMLElement;
  readonly menuKey: HTMLButtonElement;
  readonly panel: HTMLElement;
  readonly status: HTMLElement;
  readonly controlStatus: HTMLElement;
  readonly controlToggle: HTMLButtonElement;
  readonly pointerLockButton: HTMLButtonElement;
  readonly keyboardButton: HTMLButtonElement;
  readonly sendClipboardButton: HTMLButtonElement;
  readonly copyClipboardButton: HTMLButtonElement;
  readonly resetVideoButton: HTMLButtonElement;
  readonly clipboardStatus: HTMLElement;
  readonly latency: HTMLFieldSetElement;
  readonly readout: HTMLElement;
  readonly imeProxy: HTMLInputElement;
};

type Field =
  | "size"
  | "fps"
  | "bitrate"
  | "rtt"
  | "late"
  | "clock"
  | "queues"
  | "dropped"
  | "resize"
  | "codec";

const FIELDS: readonly Field[] = [
  "size",
  "fps",
  "bitrate",
  "rtt",
  "late",
  "clock",
  "queues",
  "dropped",
  "resize",
  "codec",
];

const POINTER_STILL_AFTER_MS = 2500;
const READOUT_INTERVAL_MS = 250;
const ESCAPE_CHORD_MS = 400;

function signalHeadline(state: WaywireSessionState): string {
  switch (state.video.state) {
    case "idle":
    case "connecting":
      return "Connecting to the desktop";
    case "connected":
      return "Waiting for the first frame";
    case "reconnecting":
      return "Connection lost, retrying";
    case "disconnected":
      return "Disconnected";
    case "error":
      return "Video stopped";
  }
}

function readoutField(readout: HTMLElement, name: Field): HTMLElement {
  const element = readout.querySelector(`[data-field="${name}"]`);
  if (!(element instanceof HTMLElement)) {
    throw new Error(`Expected [data-field="${name}"] in the readout`);
  }
  return element;
}

function titleFor(state: WaywireSessionState): string {
  if (state.video.state === "error") return "Waywire · fault";
  if (state.video.state === "reconnecting") return "Waywire · reconnecting";
  if (state.video.state !== "connected") return "Waywire · offline";
  if (state.input.state === "active") return "Waywire · driving";
  return "Waywire · live";
}

function inputLabel(state: WaywireSessionState): string {
  switch (state.input.state) {
    case "active":
      return "Driving";
    case "ready":
      return "Ready to drive";
    case "requesting":
    case "connecting":
      return "Requesting control";
    case "busy":
      return "Another viewer is driving";
    case "idle":
    case "disconnected":
      return "Watching";
  }
}

export function installViewerListeners(
  elements: ViewerElements,
  session: WaywireSession,
  surface: SurfaceHandle,
): () => void {
  const fields = Object.fromEntries(
    FIELDS.map((name) => [name, readoutField(elements.readout, name)]),
  ) as Record<Field, HTMLElement>;
  const cleanup: Array<() => void> = [];
  const listen = <K extends keyof HTMLElementEventMap>(
    target: HTMLElement,
    type: K,
    listener: (event: HTMLElementEventMap[K]) => void,
  ): void => {
    target.addEventListener(type, listener);
    cleanup.push(() => target.removeEventListener(type, listener));
  };
  const panelOpen = (): boolean => elements.panel.matches(":popover-open");
  const closePanel = (): void => {
    if (panelOpen()) elements.panel.hidePopover();
  };

  // The screen speaks for itself until the first frame is drawn, and again
  // whenever the feed drops.
  let framesSinceConnect = 0;
  let lastState: WaywireSessionState | null = null;
  cleanup.push(
    session.on("state", (state) => {
      lastState = state;
      if (state.video.state !== "connected") framesSinceConnect = 0;
      const live = state.video.state === "connected" && framesSinceConnect > 0;
      elements.signal.hidden = live;
      elements.signal.dataset["state"] = state.video.state;
      const headline = signalHeadline(state);
      elements.signalHeadline.textContent = headline;
      elements.signalMessage.textContent =
        state.video.message === headline ? "" : state.video.message;
      elements.status.textContent = state.video.message;
      elements.status.title = state.video.message;
      elements.status.dataset["state"] = state.video.state;
      elements.controlStatus.textContent = inputLabel(state);
      elements.controlStatus.title = state.input.message;
      elements.controlStatus.dataset["state"] = state.input.state;
      elements.stage.dataset["lease"] = state.input.state;
      elements.controlToggle.textContent =
        state.input.state === "active" ? "Release control" : "Take control";
      elements.pointerLockButton.textContent = state.input.pointerLocked
        ? "Unlock pointer"
        : "Lock pointer";
      elements.pointerLockButton.setAttribute(
        "aria-pressed",
        String(state.input.pointerLocked),
      );
      fields.codec.textContent = state.video.codec ?? "—";
      document.title = titleFor(state);
    }),
  );

  let pendingStats: WaywireStats | null = null;
  let readoutTimer: ReturnType<typeof setTimeout> | null = null;
  let lastReadout = Number.NEGATIVE_INFINITY;
  const renderReadout = (): void => {
    readoutTimer = null;
    const stats = pendingStats;
    if (!stats) return;
    pendingStats = null;
    lastReadout = performance.now();
    fields.size.textContent = `${stats.width}×${stats.height}`;
    fields.fps.textContent = `${stats.renderedFps.toFixed(0)} fps`;
    fields.bitrate.textContent = `${stats.bitrateKbps} kbps at ${stats.scalePercent}%`;
    fields.rtt.textContent = `${stats.rttMs.toFixed(1)} ms`;
    fields.late.textContent = `${stats.latenessMs.toFixed(1)} ms of ${stats.latencyTargetMs} ms`;
    fields.clock.textContent = stats.clockConfident
      ? `±${stats.clockUncertaintyMs?.toFixed(1) ?? "?"} ms`
      : "syncing";
    fields.queues.textContent = `${stats.pendingInputCount} input · ${stats.decoderQueue} decode`;
    fields.dropped.textContent = `${stats.droppedFrames} of ${stats.receivedFrames}`;
  };
  cleanup.push(
    session.on("stats", (stats) => {
      if (framesSinceConnect === 0) {
        framesSinceConnect = 1;
        elements.signal.hidden = true;
        if (lastState) document.title = titleFor(lastState);
      }
      if (!panelOpen()) return;
      pendingStats = stats;
      const remaining = READOUT_INTERVAL_MS - (performance.now() - lastReadout);
      if (remaining <= 0) {
        if (readoutTimer !== null) clearTimeout(readoutTimer);
        renderReadout();
      } else if (readoutTimer === null) {
        readoutTimer = setTimeout(renderReadout, remaining);
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
        event.width && event.height ? ` ${event.width}×${event.height}` : "";
      const latency =
        event.latencyMs === undefined
          ? ""
          : ` in ${event.latencyMs.toFixed(0)} ms`;
      fields.size.dataset["resize"] = event.state;
      fields.resize.textContent = `${event.state}${size}${latency}`;
    }),
  );
  cleanup.push(
    session.on("error", (error) => console.warn("Waywire stream error", error)),
  );

  // The SDK ties the lease to canvas focus, so the key and the panel's
  // controls never take focus: pressing them leaves the screen focused and
  // the lease where it was.
  const keepScreenFocus = (event: PointerEvent): void => event.preventDefault();
  listen(elements.menuKey, "pointerdown", keepScreenFocus);
  for (const control of elements.panel.querySelectorAll("button, label")) {
    if (control instanceof HTMLElement) {
      listen(control, "pointerdown", keepScreenFocus);
    }
  }

  listen(elements.controlToggle, "click", () => {
    if (elements.stage.dataset["lease"] === "active") {
      session.input.release();
    } else {
      session.input.acquire();
      surface.focus();
    }
    closePanel();
  });
  listen(elements.keyboardButton, "click", () => {
    session.input.acquire();
    closePanel();
    surface.focusTextInput();
  });
  listen(elements.resetVideoButton, "click", () => {
    session.video.reset();
    closePanel();
  });

  // Escape twice from the screen opens the panel and lets go of the
  // desktop; a single Escape still reaches the remote. With the panel open,
  // one Escape closes it. This runs before the SDK's own key handling.
  // While the panel is open no key reaches the remote desktop.
  let lastEscapeAt = Number.NEGATIVE_INFINITY;
  const escapeChord = (event: KeyboardEvent): void => {
    if (panelOpen()) {
      event.preventDefault();
      event.stopImmediatePropagation();
      if (event.key === "Escape" && !event.repeat) closePanel();
      return;
    }
    if (event.key !== "Escape" || event.repeat) return;
    const now = performance.now();
    if (now - lastEscapeAt <= ESCAPE_CHORD_MS) {
      lastEscapeAt = Number.NEGATIVE_INFINITY;
      event.preventDefault();
      event.stopImmediatePropagation();
      session.input.release();
      elements.panel.showPopover();
      elements.controlToggle.focus();
      return;
    }
    lastEscapeAt = now;
  };
  elements.display.addEventListener("keydown", escapeChord, { capture: true });
  cleanup.push(() =>
    elements.display.removeEventListener("keydown", escapeChord, {
      capture: true,
    }),
  );
  listen(elements.latency, "change", (event) => {
    const input = event.target;
    if (input instanceof HTMLInputElement && input.checked) {
      session.video.setLatencyTarget(Number(input.value));
    }
  });

  // The key dims while the pointer rests so it stops reading as chrome.
  let stillTimer: ReturnType<typeof setTimeout> | null = null;
  const pointerMoved = (): void => {
    elements.stage.dataset["pointer"] = "moving";
    if (stillTimer !== null) clearTimeout(stillTimer);
    stillTimer = setTimeout(() => {
      stillTimer = null;
      elements.stage.dataset["pointer"] = "still";
    }, POINTER_STILL_AFTER_MS);
  };
  listen(elements.stage, "pointermove", pointerMoved);
  listen(elements.stage, "pointerdown", pointerMoved);
  pointerMoved();

  return () => {
    if (readoutTimer !== null) clearTimeout(readoutTimer);
    if (stillTimer !== null) clearTimeout(stillTimer);
    readoutTimer = null;
    stillTimer = null;
    pendingStats = null;
    for (const remove of cleanup.splice(0).reverse()) remove();
  };
}
