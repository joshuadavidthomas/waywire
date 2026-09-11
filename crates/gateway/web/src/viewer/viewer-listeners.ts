import { Playout } from "../sdk/playout.ts";
import type {
  RemoteDisplayPolicy,
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
  readonly notice: HTMLElement;
  readonly hud: HTMLElement;
  readonly menuKey: HTMLButtonElement;
  readonly panel: HTMLElement;
  readonly status: HTMLElement;
  readonly statusVideo: HTMLElement;
  readonly controlToggle: HTMLInputElement;
  readonly pointerLockButton: HTMLButtonElement;
  readonly keyboardButton: HTMLButtonElement;
  readonly sendClipboardButton: HTMLButtonElement;
  readonly copyClipboardButton: HTMLButtonElement;
  readonly resolutionSelect: HTMLSelectElement;
  readonly resolutionLabel: HTMLLabelElement;
  readonly resetVideoButton: HTMLButtonElement;
  readonly clipboardStatus: HTMLElement;
  readonly hudToggle: HTMLInputElement;
  readonly fullscreenButton: HTMLButtonElement;
  readonly pinButton: HTMLButtonElement;
  readonly closeButton: HTMLButtonElement;
  readonly imeProxy: HTMLInputElement;
};

type Field =
  | "resolution"
  | "rate"
  | "target-bitrate"
  | "scale"
  | "rtt"
  | "late"
  | "video-latency-target"
  | "clock-uncertainty"
  | "unacknowledged-input"
  | "decode-queue"
  | "dropped"
  | "received"
  | "resize-time"
  | "codec";

const FIELDS: readonly Field[] = [
  "resolution",
  "rate",
  "target-bitrate",
  "scale",
  "rtt",
  "late",
  "video-latency-target",
  "clock-uncertainty",
  "unacknowledged-input",
  "decode-queue",
  "dropped",
  "received",
  "resize-time",
  "codec",
];

const POINTER_STILL_AFTER_MS = 2500;
const HUD_INTERVAL_MS = 250;
const HUD_IDLE_AFTER_MS = 1500;
const ESCAPE_CHORD_MS = 400;
const HUD_STORAGE_KEY = "waywire.hud";
const PANEL_STORAGE_KEY = "waywire.panel";
export const RESOLUTION_STORAGE_KEY = "waywire.resolution";
export const RESOLUTION_VALUES = [
  "fit",
  "2560x1440",
  "1920x1080",
  "1600x900",
  "1366x768",
  "1280x720",
  "1024x768",
] as const;

type Resolution = (typeof RESOLUTION_VALUES)[number];
type FixedResolution = Exclude<Resolution, "fit">;
type Wheel = "auto" | "handsOff";
type Panel = "floating" | "pinned";

// A menu item is an icon and a label, so its wording lives in the label span.
function itemLabel(item: HTMLElement): HTMLElement {
  const label = item.querySelector(".label");
  if (!(label instanceof HTMLElement)) {
    throw new Error(`Expected a .label inside #${item.id}`);
  }
  return label;
}

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
      return "Connecting to the remote";
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
  if (state.video.state === "error") return "Waywire (video stopped)";
  if (state.video.state === "reconnecting") return "Waywire (reconnecting)";
  if (state.video.state !== "connected") return "Waywire (not connected)";
  if (state.input.state === "busy") return "Waywire (in use)";
  return "Waywire";
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

function parseResolution(value: string | null): Resolution | null {
  for (const allowed of RESOLUTION_VALUES) {
    if (value === allowed) return allowed;
  }
  return null;
}

const FIXED_RESOLUTIONS: Record<
  FixedResolution,
  readonly [width: number, height: number]
> = {
  "2560x1440": [2560, 1440],
  "1920x1080": [1920, 1080],
  "1600x900": [1600, 900],
  "1366x768": [1366, 768],
  "1280x720": [1280, 720],
  "1024x768": [1024, 768],
};

function resolutionPolicy(
  resolution: Resolution,
  display: HTMLCanvasElement,
): RemoteDisplayPolicy {
  if (resolution === "fit") {
    return {
      mode: "observe",
      element: display,
      devicePixelRatio: 1,
    };
  }
  const [width, height] = FIXED_RESOLUTIONS[resolution];
  return { mode: "fixed", width, height, scale: 1 };
}

// What the page keeps hold of: a way to let an action settle the menu the way
// the viewer asked for it, and a way to tear the whole thing down.
export type ViewerControls = {
  readonly settle: () => void;
  readonly dispose: () => void;
};

export function installViewerListeners(
  elements: ViewerElements,
  session: WaywireSession,
  surface: SurfaceHandle,
): ViewerControls {
  const storedResolution = readStored(RESOLUTION_STORAGE_KEY);
  let resolution = parseResolution(storedResolution) ?? "fit";
  elements.resolutionSelect.value = resolution;
  session.remoteDisplay.setPolicy(
    resolutionPolicy(resolution, elements.display),
  );
  if (storedResolution !== null && storedResolution !== resolution) {
    writeStored(RESOLUTION_STORAGE_KEY, resolution);
  }

  const fields = Object.fromEntries(
    FIELDS.map((name) => [name, hudField(elements.hud, name)]),
  ) as Record<Field, HTMLElement>;
  const pointerLockLabel = itemLabel(elements.pointerLockButton);
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
  // that stays and lets the remote through).
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

  // Reaching the remote is seamless. The SDK ties input ownership to canvas
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
  const keyboardTitle = elements.keyboardButton.title;
  const renderWheel = (): void => {
    const inputEnabled = wheel === "auto";
    elements.controlToggle.checked = inputEnabled;
    elements.keyboardButton.disabled = !inputEnabled;
    elements.keyboardButton.title = inputEnabled
      ? keyboardTitle
      : "Turn on Remote input first";
  };
  // The page's one notice, for the two states that explain a screen which
  // does not answer. It reports the situation; the switch names the action.
  const renderNotice = (state: WaywireSessionState): void => {
    if (wheel === "handsOff") {
      elements.notice.textContent = "View only";
      elements.notice.hidden = false;
    } else if (state.input.state === "busy") {
      elements.notice.textContent = "Another viewer has control";
      elements.notice.hidden = false;
    } else {
      elements.notice.hidden = true;
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
      renderNotice(state);
      elements.status.dataset["state"] = state.video.state;
      elements.status.title = state.video.message;
      elements.statusVideo.textContent = videoLabel(state);
      pointerLockLabel.textContent = state.input.pointerLocked
        ? "Unlock pointer"
        : "Lock pointer";
      fields.codec.textContent = state.video.codec ?? "-";
      document.title = titleFor(state);
    }),
  );

  // Stats live in their own overlay. Frames only arrive while the remote
  // changes, so the overlay renders the last snapshot on switch-on and marks
  // the rate idle once frames stop.
  const playout = new Playout();
  let lastStats: WaywireStats | null = null;
  let lastStatsAt = Number.NEGATIVE_INFINITY;
  let pendingStats: WaywireStats | null = null;
  let hudTimer: ReturnType<typeof setTimeout> | null = null;
  let lastHud = Number.NEGATIVE_INFINITY;
  const hudVisible = (): boolean => !elements.hud.hidden;
  const renderStats = (stats: WaywireStats, idle: boolean): void => {
    fields.resolution.textContent = `${stats.width}×${stats.height}`;
    fields.rate.textContent = idle
      ? "idle"
      : `${stats.renderedFps.toFixed(0)} fps`;
    fields["target-bitrate"].textContent = `${stats.bitrateKbps} kbps`;
    fields.scale.textContent = `${stats.scalePercent}%`;
    fields.rtt.textContent = `${stats.rttMs.toFixed(1)} ms`;
    fields.late.textContent = `${stats.latenessMs.toFixed(1)} ms`;
    fields["video-latency-target"].textContent = `${stats.latencyTargetMs} ms`;
    fields["clock-uncertainty"].textContent = stats.clockConfident
      ? `±${stats.clockUncertaintyMs?.toFixed(1) ?? "?"} ms`
      : "syncing";
    fields["unacknowledged-input"].textContent = String(
      stats.pendingInputCount,
    );
    fields["decode-queue"].textContent = String(stats.decoderQueue);
    fields.dropped.textContent = String(stats.droppedFrames);
    fields.received.textContent = String(stats.receivedFrames);
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
      fields.rate.textContent = "idle";
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
      const target = playout.update(
        stats.clockConfident
          ? { latenessMs: stats.latenessMs, decodeQueue: stats.decoderQueue }
          : null,
        stats.drawCompletedAtMs,
      );
      if (target !== stats.latencyTargetMs)
        session.video.setLatencyTarget(target);
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
      const empty = event.text === null || event.text.length === 0;
      elements.copyClipboardButton.disabled = empty;
      elements.copyClipboardButton.title = empty
        ? "The remote clipboard is empty"
        : "Bring the remote clipboard here";
      if (event.text !== null) elements.clipboardStatus.textContent = "";
    }),
  );
  cleanup.push(
    session.on("resize", (event) => {
      fields.resolution.dataset["resize"] = event.state;
      if (event.state === "presented" && event.latencyMs !== undefined) {
        fields["resize-time"].textContent = `${event.latencyMs.toFixed(0)} ms`;
      }
    }),
  );
  cleanup.push(
    session.on("error", (error) => console.warn("Waywire stream error", error)),
  );

  // Buttons and switches keep input ownership on the screen. The resolution
  // label stays native so pointer and keyboard users can focus and open its
  // select without a prevented pointerdown cancelling the browser action.
  const keepScreenFocus = (event: PointerEvent): void => event.preventDefault();
  listen(elements.menuKey, "pointerdown", keepScreenFocus);
  for (const control of elements.panel.querySelectorAll("button, label")) {
    if (
      control instanceof HTMLElement &&
      control !== elements.resolutionLabel
    ) {
      listen(control, "pointerdown", keepScreenFocus);
    }
  }

  listen(elements.controlToggle, "change", () => {
    if (elements.controlToggle.checked) {
      wheel = "auto";
      session.input.acquire();
      surface.focus();
    } else {
      wheel = "handsOff";
      session.input.release();
      elements.display.blur();
    }
    renderWheel();
    if (lastState) renderNotice(lastState);
    settle();
  });
  listen(elements.keyboardButton, "click", () => {
    if (wheel === "handsOff") return;
    session.input.acquire();
    settle();
    surface.focusTextInput();
  });
  listen(elements.resolutionSelect, "change", () => {
    const selected = parseResolution(elements.resolutionSelect.value);
    if (selected === null) {
      elements.resolutionSelect.value = resolution;
      return;
    }
    resolution = selected;
    session.remoteDisplay.setPolicy(
      resolutionPolicy(resolution, elements.display),
    );
    writeStored(RESOLUTION_STORAGE_KEY, resolution);
    // The native select takes focus and releases canvas ownership. Request it
    // for this explicit action so the saved policy is sent on the active reply.
    // Keep focus on the select so keyboard users can continue choosing options.
    if (wheel === "auto") session.input.acquire();
  });
  listen(elements.hudToggle, "change", () =>
    setHud(elements.hudToggle.checked),
  );
  listen(elements.resetVideoButton, "click", () => {
    session.video.reset();
    settle();
  });
  // Closing by hand ends the request to keep the menu open. Reopening it
  // later should not surprise you with a menu that will not go away.
  listen(elements.closeButton, "click", () => {
    closePanel();
    if (panel === "pinned") {
      panel = "floating";
      writeStored(PANEL_STORAGE_KEY, panel);
      applyPanelMode();
    }
  });
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

  return {
    settle,
    dispose: () => {
      if (hudTimer !== null) clearTimeout(hudTimer);
      if (stillTimer !== null) clearTimeout(stillTimer);
      hudTimer = null;
      stillTimer = null;
      pendingStats = null;
      for (const remove of cleanup.splice(0).reverse()) remove();
    },
  };
}
