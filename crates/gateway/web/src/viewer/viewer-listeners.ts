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
  readonly leaseNotice: HTMLElement;
  readonly hud: HTMLElement;
  readonly menuKey: HTMLButtonElement;
  readonly panel: HTMLElement;
  readonly status: HTMLElement;
  readonly statusVideo: HTMLElement;
  readonly controlStatus: HTMLElement;
  readonly controlToggle: HTMLButtonElement;
  readonly pointerLockButton: HTMLButtonElement;
  readonly keyboardButton: HTMLButtonElement;
  readonly sendClipboardButton: HTMLButtonElement;
  readonly copyClipboardButton: HTMLButtonElement;
  readonly resetVideoButton: HTMLButtonElement;
  readonly clipboardStatus: HTMLElement;
  readonly hudToggle: HTMLInputElement;
  readonly latency: HTMLElement;
  readonly fullscreenButton: HTMLButtonElement;
  readonly pinButton: HTMLButtonElement;
  readonly closeButton: HTMLButtonElement;
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
const HUD_INTERVAL_MS = 250;
const HUD_IDLE_AFTER_MS = 1500;
const ESCAPE_CHORD_MS = 400;
const HUD_STORAGE_KEY = "waywire.hud";
const PANEL_STORAGE_KEY = "waywire.panel";
export const LATENCY_STORAGE_KEY = "waywire.latency";

type Wheel = "auto" | "handsOff";
type Panel = "floating" | "pinned";

function hudField(hud: HTMLElement, name: Field): HTMLElement {
  const element = hud.querySelector(`[data-field="${name}"]`);
  if (!(element instanceof HTMLElement)) {
    throw new Error(`Expected [data-field="${name}"] in the stats readout`);
  }
  return element;
}

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

// Say what is true of the stream, not how exciting it is.
function videoLabel(state: WaywireSessionState): string {
  switch (state.video.state) {
    case "idle":
    case "connecting":
      return "Connecting";
    case "connected":
      return "Connected";
    case "reconnecting":
      return "Reconnecting";
    case "disconnected":
      return "Not connected";
    case "error":
      return "Video stopped";
  }
}

function titleFor(state: WaywireSessionState): string {
  if (state.video.state === "error") return "Waywire · video stopped";
  if (state.video.state === "reconnecting") return "Waywire · reconnecting";
  if (state.video.state !== "connected") return "Waywire · not connected";
  if (state.input.state === "busy") return "Waywire · someone else has it";
  return "Waywire · connected";
}

// The second half of the status line answers the other question a person
// has: do my mouse and keyboard reach that desktop?
function inputLabel(state: WaywireSessionState): string {
  switch (state.input.state) {
    case "active":
      return "yours to use";
    case "requesting":
    case "connecting":
      return "claiming it";
    case "busy":
      return "someone else has it";
    case "ready":
    case "idle":
    case "disconnected":
      return "watching only";
  }
}

function readStored(key: string): string | null {
  try {
    return localStorage.getItem(key);
  } catch {
    return null;
  }
}

function writeStored(key: string, value: string): void {
  try {
    localStorage.setItem(key, value);
  } catch {
    // A preference that cannot be stored still applies for this page.
  }
}

export function installViewerListeners(
  elements: ViewerElements,
  session: WaywireSession,
  surface: SurfaceHandle,
): () => void {
  const fields = Object.fromEntries(
    FIELDS.map((name) => [name, hudField(elements.hud, name)]),
  ) as Record<Field, HTMLElement>;
  const cleanup: Array<() => void> = [];
  const listen = <K extends keyof HTMLElementEventMap>(
    target: HTMLElement,
    type: K,
    listener: (event: HTMLElementEventMap[K]) => void,
    options?: AddEventListenerOptions,
  ): void => {
    target.addEventListener(type, listener, options);
    cleanup.push(() => target.removeEventListener(type, listener, options));
  };
  const listenDocument = <K extends keyof DocumentEventMap>(
    type: K,
    listener: (event: DocumentEventMap[K]) => void,
  ): void => {
    document.addEventListener(type, listener);
    cleanup.push(() => document.removeEventListener(type, listener));
  };

  // Menu: floating (light dismiss, scrim, keys held) or pinned (a palette
  // that stays and lets the desktop through).
  let panel: Panel =
    readStored(PANEL_STORAGE_KEY) === "pinned" ? "pinned" : "floating";
  const panelOpen = (): boolean => elements.panel.matches(":popover-open");
  const applyPanelMode = (): void => {
    elements.stage.dataset["panel"] = panel;
    elements.pinButton.setAttribute("aria-pressed", String(panel === "pinned"));
    const wasOpen = panelOpen();
    if (wasOpen) elements.panel.hidePopover();
    elements.panel.setAttribute(
      "popover",
      panel === "pinned" ? "manual" : "auto",
    );
    if (wasOpen) elements.panel.showPopover();
  };
  const closePanel = (): void => {
    if (panelOpen()) elements.panel.hidePopover();
  };
  // Actions close a floating panel; a pinned one stays.
  const settle = (): void => {
    if (panel === "floating") closePanel();
  };
  applyPanelMode();
  if (panel === "pinned") elements.panel.showPopover();

  // Reaching the desktop is seamless. The SDK ties the lease to canvas
  // focus, so the screen takes focus whenever the pointer is over it or the
  // window comes back, unless the viewer switched their input off.
  let wheel: Wheel = "auto";
  const focusScreen = (): void => {
    if (wheel === "handsOff") return;
    if (panel === "floating" && panelOpen()) return;
    const active = document.activeElement;
    if (active === elements.display || active === elements.imeProxy) return;
    if (active instanceof HTMLElement && elements.panel.contains(active))
      return;
    elements.display.focus({ preventScroll: true });
  };
  listen(elements.display, "pointerenter", focusScreen);
  listen(elements.display, "pointermove", focusScreen);
  listen(elements.display, "pointerdown", focusScreen);
  const onWindowFocus = (): void => focusScreen();
  window.addEventListener("focus", onWindowFocus);
  cleanup.push(() => window.removeEventListener("focus", onWindowFocus));
  const renderWheel = (): void => {
    elements.controlToggle.setAttribute(
      "aria-pressed",
      String(wheel === "auto"),
    );
  };
  // The one mode a person forgets they are in gets the page's one pill.
  const renderLeaseNotice = (state: WaywireSessionState): void => {
    if (wheel === "handsOff") {
      elements.leaseNotice.textContent = "Your mouse and keyboard are off";
      elements.leaseNotice.hidden = false;
    } else if (state.input.state === "busy") {
      elements.leaseNotice.textContent = "Someone else has this desktop";
      elements.leaseNotice.hidden = false;
    } else {
      elements.leaseNotice.hidden = true;
    }
  };
  renderWheel();

  let lastState: WaywireSessionState | null = null;
  let framesSinceConnect = 0;
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
      renderLeaseNotice(state);
      elements.status.dataset["state"] = state.video.state;
      elements.status.title = state.video.message;
      elements.statusVideo.textContent = videoLabel(state);
      elements.controlStatus.textContent = inputLabel(state);
      elements.controlStatus.title = state.input.message;
      elements.controlStatus.dataset["state"] = state.input.state;
      elements.pointerLockButton.setAttribute(
        "aria-pressed",
        String(state.input.pointerLocked),
      );
      fields.codec.textContent = state.video.codec ?? "—";
      document.title = titleFor(state);
    }),
  );

  // Stats live in their own overlay. Frames only arrive while the desktop
  // changes, so the overlay renders the last snapshot on switch-on and marks
  // the rate idle once frames stop.
  let lastStats: WaywireStats | null = null;
  let lastStatsAt = Number.NEGATIVE_INFINITY;
  let pendingStats: WaywireStats | null = null;
  let hudTimer: ReturnType<typeof setTimeout> | null = null;
  let lastHud = Number.NEGATIVE_INFINITY;
  const hudVisible = (): boolean => !elements.hud.hidden;
  const renderStats = (stats: WaywireStats, idle: boolean): void => {
    fields.size.textContent = `${stats.width}×${stats.height}`;
    fields.fps.textContent = idle
      ? "idle"
      : `${stats.renderedFps.toFixed(0)} fps`;
    fields.bitrate.textContent = `${stats.bitrateKbps} kbps at ${stats.scalePercent}%`;
    fields.rtt.textContent = `${stats.rttMs.toFixed(1)} ms`;
    fields.late.textContent = `${stats.latenessMs.toFixed(1)} ms of ${stats.latencyTargetMs} ms`;
    fields.clock.textContent = stats.clockConfident
      ? `±${stats.clockUncertaintyMs?.toFixed(1) ?? "?"} ms`
      : "syncing";
    fields.queues.textContent = `${stats.pendingInputCount} input · ${stats.decoderQueue} decode`;
    fields.dropped.textContent = `${stats.droppedFrames} of ${stats.receivedFrames}`;
  };
  const renderHud = (): void => {
    hudTimer = null;
    const stats = pendingStats;
    if (!stats) return;
    pendingStats = null;
    lastHud = performance.now();
    renderStats(stats, false);
  };
  const idleTicker = setInterval(() => {
    if (!hudVisible() || !lastStats) return;
    if (performance.now() - lastStatsAt > HUD_IDLE_AFTER_MS) {
      fields.fps.textContent = "idle";
    }
  }, 1000);
  cleanup.push(() => clearInterval(idleTicker));
  const setHud = (visible: boolean): void => {
    elements.hud.hidden = !visible;
    elements.hudToggle.checked = visible;
    writeStored(HUD_STORAGE_KEY, visible ? "on" : "off");
    if (visible && lastStats) {
      renderStats(
        lastStats,
        performance.now() - lastStatsAt > HUD_IDLE_AFTER_MS,
      );
    }
  };
  setHud(readStored(HUD_STORAGE_KEY) === "on");
  cleanup.push(
    session.on("stats", (stats) => {
      lastStats = stats;
      lastStatsAt = performance.now();
      if (framesSinceConnect === 0) {
        framesSinceConnect = 1;
        elements.signal.hidden = true;
        if (lastState) document.title = titleFor(lastState);
      }
      if (!hudVisible()) return;
      pendingStats = stats;
      const remaining = HUD_INTERVAL_MS - (performance.now() - lastHud);
      if (remaining <= 0) {
        if (hudTimer !== null) clearTimeout(hudTimer);
        renderHud();
      } else if (hudTimer === null) {
        hudTimer = setTimeout(renderHud, remaining);
      }
    }),
  );
  cleanup.push(
    session.on("clipboard", (event) => {
      elements.copyClipboardButton.disabled =
        event.text === null || event.text.length === 0;
      if (event.text !== null) elements.clipboardStatus.textContent = "";
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

  // The key and the menu's controls never take focus, so pressing them
  // leaves the screen focused and the lease where it was.
  const keepScreenFocus = (event: PointerEvent): void => event.preventDefault();
  listen(elements.menuKey, "pointerdown", keepScreenFocus);
  for (const control of elements.panel.querySelectorAll("button, label")) {
    if (control instanceof HTMLElement) {
      listen(control, "pointerdown", keepScreenFocus);
    }
  }

  listen(elements.controlToggle, "click", () => {
    if (wheel === "handsOff") {
      wheel = "auto";
      session.input.acquire();
      surface.focus();
    } else {
      wheel = "handsOff";
      session.input.release();
      elements.display.blur();
    }
    renderWheel();
    if (lastState) renderLeaseNotice(lastState);
    settle();
  });
  listen(elements.keyboardButton, "click", () => {
    wheel = "auto";
    renderWheel();
    if (lastState) renderLeaseNotice(lastState);
    session.input.acquire();
    settle();
    surface.focusTextInput();
  });
  listen(elements.hudToggle, "change", () =>
    setHud(elements.hudToggle.checked),
  );
  listen(elements.resetVideoButton, "click", () => {
    session.video.reset();
    settle();
  });
  listen(elements.latency, "change", (event) => {
    const input = event.target;
    if (input instanceof HTMLInputElement && input.checked) {
      session.video.setLatencyTarget(Number(input.value));
      writeStored(LATENCY_STORAGE_KEY, input.value);
    }
  });
  listen(elements.closeButton, "click", closePanel);
  listen(elements.pinButton, "click", () => {
    panel = panel === "pinned" ? "floating" : "pinned";
    writeStored(PANEL_STORAGE_KEY, panel);
    applyPanelMode();
  });
  listen(elements.fullscreenButton, "click", () => {
    if (document.fullscreenElement) {
      void document.exitFullscreen();
    } else {
      void document.documentElement.requestFullscreen();
    }
  });
  listenDocument("fullscreenchange", () => {
    elements.fullscreenButton.setAttribute(
      "aria-pressed",
      String(document.fullscreenElement !== null),
    );
  });

  // Escape twice from the screen opens the menu without the pointer; a
  // single Escape still reaches the remote. With a floating menu open no key
  // reaches the remote and one Escape closes it; a pinned menu lets keys
  // through.
  let lastEscapeAt = Number.NEGATIVE_INFINITY;
  const escapeChord = (event: KeyboardEvent): void => {
    if (panel === "floating" && panelOpen()) {
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
      if (!panelOpen()) elements.panel.showPopover();
      elements.controlToggle.focus();
      return;
    }
    lastEscapeAt = now;
  };
  listen(elements.display, "keydown", escapeChord, { capture: true });

  // The key fades while the pointer rests so it stops reading as chrome.
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
    if (hudTimer !== null) clearTimeout(hudTimer);
    if (stillTimer !== null) clearTimeout(stillTimer);
    hudTimer = null;
    stillTimer = null;
    pendingStats = null;
    for (const remove of cleanup.splice(0).reverse()) remove();
  };
}
