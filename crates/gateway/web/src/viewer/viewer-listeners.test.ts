import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";

import type {
  RemoteDisplayPolicy,
  SurfaceHandle,
  WaywireSession,
  WaywireStats,
} from "../sdk/waywire.ts";
import {
  installViewerListeners,
  RESOLUTION_STORAGE_KEY,
  RESOLUTION_VALUES,
  type ViewerControls,
  type ViewerElements,
} from "./viewer-listeners.ts";

class FakeElement {
  readonly dataset: Record<string, string> = {};
  readonly attributes = new Map<string, string>();
  readonly listeners = new Map<string, Set<EventListener>>();
  readonly queryResults = new Map<string, FakeElement>();
  readonly queryAllResults = new Map<string, FakeElement[]>();
  checked = false;
  disabled = false;
  hidden = false;
  popoverOpen = false;
  textContent: string | null = "";
  title = "";

  addEventListener(
    type: string,
    listener: EventListenerOrEventListenerObject,
  ): void {
    if (typeof listener !== "function") return;
    let listeners = this.listeners.get(type);
    if (!listeners) this.listeners.set(type, (listeners = new Set()));
    listeners.add(listener);
  }

  removeEventListener(
    type: string,
    listener: EventListenerOrEventListenerObject,
  ): void {
    if (typeof listener === "function") {
      this.listeners.get(type)?.delete(listener);
    }
  }

  dispatch(type: string): FakeEvent {
    const event = new FakeEvent(this);
    for (const listener of this.listeners.get(type) ?? []) {
      listener(event as unknown as Event);
    }
    return event;
  }

  querySelector<T extends Element = Element>(selector: string): T | null {
    return (this.queryResults.get(selector) ?? null) as T | null;
  }

  querySelectorAll<T extends Element = Element>(
    selector: string,
  ): NodeListOf<T> {
    return (this.queryAllResults.get(selector) ??
      []) as unknown as NodeListOf<T>;
  }

  matches(selector: string): boolean {
    return selector === ":popover-open" && this.popoverOpen;
  }

  contains(element: Node | null): boolean {
    return element === (this as unknown as Node);
  }

  setAttribute(name: string, value: string): void {
    this.attributes.set(name, value);
  }

  hidePopover(): void {
    this.popoverOpen = false;
  }

  showPopover(): void {
    this.popoverOpen = true;
  }

  focus(_options?: FocusOptions): void {
    fakeDocument.activeElement = this as unknown as Element;
  }

  blur(): void {
    if (fakeDocument.activeElement === (this as unknown as Element)) {
      fakeDocument.activeElement = null;
    }
  }
}

class FakeEvent {
  defaultPrevented = false;

  constructor(readonly target: FakeElement) {}

  preventDefault(): void {
    this.defaultPrevented = true;
  }
}

class FakeInput extends FakeElement {
  value = "";
}

class FakeSelect extends FakeElement {
  value = "";
}

class FakeStorage {
  readonly values: Map<string, string>;

  constructor(initial: Readonly<Record<string, string>> = {}) {
    this.values = new Map(Object.entries(initial));
  }

  getItem(key: string): string | null {
    return this.values.get(key) ?? null;
  }

  setItem(key: string, value: string): void {
    this.values.set(key, value);
  }
}

class FakeDocument extends FakeElement {
  activeElement: Element | null = null;
  fullscreenElement: Element | null = null;
  title = "";
  readonly documentElement = Object.assign(new FakeElement(), {
    requestFullscreen: () => Promise.resolve(),
  });
}

let fakeDocument = new FakeDocument();

function installDom(
  stored: Readonly<Record<string, string>> = {},
): FakeStorage {
  fakeDocument = new FakeDocument();
  const storage = new FakeStorage(stored);
  Object.defineProperties(globalThis, {
    HTMLElement: { configurable: true, value: FakeElement },
    HTMLInputElement: { configurable: true, value: FakeInput },
    HTMLSelectElement: { configurable: true, value: FakeSelect },
    document: { configurable: true, value: fakeDocument },
    window: { configurable: true, value: new FakeElement() },
    localStorage: { configurable: true, value: storage },
  });
  return storage;
}

const FIELD_NAMES = [
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
] as const;

function viewerElements(): {
  readonly elements: ViewerElements;
  readonly controlToggle: FakeInput;
  readonly keyboardButton: FakeElement;
  readonly protectedLabel: FakeElement;
  readonly resolutionLabel: FakeElement;
  readonly resolutionSelect: FakeSelect;
} {
  const element = (): FakeElement => new FakeElement();
  const hud = element();
  for (const name of FIELD_NAMES) {
    hud.queryResults.set(`[data-field="${name}"]`, element());
  }
  const pointerLockButton = element();
  pointerLockButton.queryResults.set(".label", element());
  const controlToggle = new FakeInput();
  controlToggle.checked = true;
  const keyboardButton = element();
  keyboardButton.title = "Type into the remote";
  const panel = element();
  const protectedLabel = element();
  const resolutionLabel = element();
  panel.queryAllResults.set("button, label", [protectedLabel, resolutionLabel]);
  const resolutionSelect = new FakeSelect();
  return {
    elements: {
      stage: element(),
      display: element(),
      signal: element(),
      signalHeadline: element(),
      signalMessage: element(),
      notice: element(),
      hud,
      menuKey: element(),
      panel,
      status: element(),
      statusVideo: element(),
      controlToggle,
      pointerLockButton,
      keyboardButton,
      sendClipboardButton: element(),
      copyClipboardButton: element(),
      resolutionSelect,
      resolutionLabel,
      resetVideoButton: element(),
      clipboardStatus: element(),
      hudToggle: new FakeInput(),
      fullscreenButton: element(),
      pinButton: element(),
      closeButton: element(),
      imeProxy: new FakeInput(),
    } as unknown as ViewerElements,
    controlToggle,
    keyboardButton,
    protectedLabel,
    resolutionLabel,
    resolutionSelect,
  };
}

function setup(stored: Readonly<Record<string, string>> = {}): {
  readonly controls: ViewerControls;
  readonly elements: ViewerElements;
  readonly controlToggle: FakeInput;
  readonly keyboardButton: FakeElement;
  readonly protectedLabel: FakeElement;
  readonly resolutionLabel: FakeElement;
  readonly resolutionSelect: FakeSelect;
  readonly storage: FakeStorage;
  readonly emitStats: (stats: WaywireStats) => void;
  readonly calls: {
    acquired: number;
    released: number;
    focused: number;
    textFocused: number;
    policies: RemoteDisplayPolicy[];
    targets: number[];
  };
} {
  const storage = installDom(stored);
  const created = viewerElements();
  const { elements, controlToggle, keyboardButton } = created;
  const calls = {
    acquired: 0,
    released: 0,
    focused: 0,
    textFocused: 0,
    policies: [] as RemoteDisplayPolicy[],
    targets: [] as number[],
  };
  const statsListeners = new Set<(stats: WaywireStats) => void>();
  const session = {
    input: {
      acquire: () => {
        calls.acquired += 1;
      },
      release: () => {
        calls.released += 1;
      },
    },
    video: {
      reset: () => undefined,
      setLatencyTarget: (target: number) => calls.targets.push(target),
    },
    remoteDisplay: {
      setPolicy: (policy: RemoteDisplayPolicy) => {
        calls.policies.push(policy);
      },
    },
    on: (type: string, listener: (stats: WaywireStats) => void) => {
      if (type === "stats") statsListeners.add(listener);
      return () => statsListeners.delete(listener);
    },
  } as unknown as WaywireSession;
  const surface = {
    focus: () => {
      calls.focused += 1;
    },
    focusTextInput: () => {
      calls.textFocused += 1;
    },
  } as unknown as SurfaceHandle;
  return {
    controls: installViewerListeners(elements, session, surface),
    elements,
    controlToggle,
    keyboardButton,
    protectedLabel: created.protectedLabel,
    resolutionLabel: created.resolutionLabel,
    resolutionSelect: created.resolutionSelect,
    storage,
    emitStats: (stats) => {
      for (const listener of statsListeners) listener(stats);
    },
    calls,
  };
}

test("playout adapts through the SDK setter even with the stats readout hidden", () => {
  const harness = setup();
  const sample: WaywireStats = {
    width: 1280,
    height: 720,
    renderedFps: 60,
    renderedMediaTimestampMicros: 0,
    drawCompletedAtMs: 0,
    generation: 1,
    bitrateKbps: 8000,
    scalePercent: 100,
    rttMs: 20,
    clockConfident: true,
    clockUncertaintyMs: 1,
    latencyTargetMs: 100,
    latenessMs: 30,
    pendingInputCount: 0,
    decoderQueue: 0,
    receivedFrames: 1,
    decodedFrames: 1,
    presentedFrames: 1,
    droppedFrames: 0,
    overdueDroppedFrames: 0,
    decodedOverflowDroppedFrames: 0,
    decoderResetDroppedFrames: 0,
    decoderResets: 0,
    resizeState: "idle",
  };
  try {
    assert.equal(harness.elements.hud.hidden, true);
    for (let frame = 0; frame < 3; frame += 1)
      harness.emitStats({ ...sample, drawCompletedAtMs: frame * 16 });
    assert.deepEqual(harness.calls.targets, []);
    harness.emitStats({ ...sample, drawCompletedAtMs: 48 });
    assert.deepEqual(harness.calls.targets, [125]);
    // Untimed frames cannot build a false calm streak while the clock is syncing.
    for (let frame = 0; frame < 120; frame += 1)
      harness.emitStats({
        ...sample,
        clockConfident: false,
        latencyTargetMs: 125,
        drawCompletedAtMs: 50 + frame * 16,
      });
    assert.deepEqual(harness.calls.targets, [125]);
    for (let frame = 0; frame < 120; frame += 1)
      harness.emitStats({
        ...sample,
        latenessMs: 0,
        latencyTargetMs: 125,
        drawCompletedAtMs: 6_000 + frame * 16,
      });
    assert.deepEqual(harness.calls.targets, [125, 100]);
    assert.equal(harness.storage.getItem("waywire.latency"), null);
    harness.controls.dispose();
    for (let frame = 0; frame < 4; frame += 1)
      harness.emitStats({ ...sample, drawCompletedAtMs: 12_000 + frame * 16 });
    assert.deepEqual(harness.calls.targets, [125, 100]);
  } finally {
    harness.controls.dispose();
  }
});

test("resolution options use the exact value order and display labels", () => {
  const html = readFileSync(
    new URL("../../index.html", import.meta.url),
    "utf8",
  );
  const select = html.match(
    /<select[\s\S]*?id="resolution"[\s\S]*?>([\s\S]*?)<\/select>/,
  )?.[1];
  assert.ok(select, "resolution select was not found");
  const options = [
    ...select.matchAll(/<option value="([^"]+)">([^<]+)<\/option>/g),
  ].map((match) => [match[1], match[2]]);

  assert.deepEqual(
    options.map(([value]) => value),
    RESOLUTION_VALUES,
  );
  assert.deepEqual(
    options.map(([, label]) => label),
    [
      "Fit to window",
      "2560 × 1440",
      "1920 × 1080",
      "1600 × 900",
      "1366 × 768",
      "1280 × 720",
      "1024 × 768",
    ],
  );
  assert.doesNotMatch(html, /id="latency"|name="latency"/);
});

test("stored fixed resolution is restored before viewer actions", () => {
  const harness = setup({ [RESOLUTION_STORAGE_KEY]: "1920x1080" });
  try {
    assert.equal(harness.resolutionSelect.value, "1920x1080");
    assert.deepEqual(harness.calls.policies, [
      {
        mode: "fixed",
        width: 1920,
        height: 1080,
        scale: 1,
      },
    ]);
  } finally {
    harness.controls.dispose();
  }
});

test("resolution changes set exact fixed and Fit policies", () => {
  const harness = setup();
  try {
    assert.equal(harness.resolutionSelect.value, "fit");
    assert.deepEqual(harness.calls.policies, [
      {
        mode: "observe",
        element: harness.elements.display,
        devicePixelRatio: 1,
      },
    ]);

    harness.resolutionSelect.value = "2560x1440";
    harness.resolutionSelect.dispatch("change");
    assert.deepEqual(harness.calls.policies.at(-1), {
      mode: "fixed",
      width: 2560,
      height: 1440,
      scale: 1,
    });
    assert.equal(harness.storage.getItem(RESOLUTION_STORAGE_KEY), "2560x1440");

    harness.resolutionSelect.value = "fit";
    harness.resolutionSelect.dispatch("change");
    assert.deepEqual(harness.calls.policies.at(-1), {
      mode: "observe",
      element: harness.elements.display,
      devicePixelRatio: 1,
    });
    assert.equal(harness.storage.getItem(RESOLUTION_STORAGE_KEY), "fit");
    assert.equal(
      harness.calls.acquired,
      2,
      "each valid selection can reacquire ownership",
    );
    assert.equal(
      harness.calls.focused,
      0,
      "selection keeps native keyboard focus",
    );

    harness.resolutionSelect.value = "800x600";
    harness.resolutionSelect.dispatch("change");
    assert.equal(harness.resolutionSelect.value, "fit");
    assert.equal(harness.calls.policies.length, 3);
    assert.equal(harness.storage.getItem(RESOLUTION_STORAGE_KEY), "fit");
  } finally {
    harness.controls.dispose();
  }
});

test("stored values outside the exact list fall back to Fit", () => {
  const harness = setup({ [RESOLUTION_STORAGE_KEY]: "1920 × 1080" });
  try {
    assert.equal(harness.resolutionSelect.value, "fit");
    assert.deepEqual(harness.calls.policies, [
      {
        mode: "observe",
        element: harness.elements.display,
        devicePixelRatio: 1,
      },
    ]);
    assert.equal(harness.storage.getItem(RESOLUTION_STORAGE_KEY), "fit");
  } finally {
    harness.controls.dispose();
  }
});

test("resolution label keeps its native pointer action", () => {
  const harness = setup();
  try {
    assert.equal(
      harness.protectedLabel.dispatch("pointerdown").defaultPrevented,
      true,
    );
    assert.equal(
      harness.resolutionLabel.dispatch("pointerdown").defaultPrevented,
      false,
    );
  } finally {
    harness.controls.dispose();
  }
});

test("on-screen keyboard stays inert while Remote input is off", () => {
  const harness = setup();
  try {
    harness.controlToggle.checked = false;
    harness.controlToggle.dispatch("change");

    assert.equal(harness.keyboardButton.disabled, true);
    assert.equal(harness.keyboardButton.title, "Turn on Remote input first");
    harness.keyboardButton.dispatch("click");
    assert.equal(harness.controlToggle.checked, false);
    assert.equal(harness.keyboardButton.disabled, true);
    assert.equal(harness.calls.acquired, 0);
    assert.equal(harness.calls.focused, 0);
    assert.equal(harness.calls.textFocused, 0);
    assert.equal(harness.calls.released, 1);
    harness.resolutionSelect.value = "1280x720";
    harness.resolutionSelect.dispatch("change");
    assert.equal(
      harness.calls.acquired,
      0,
      "changing a preference must not turn input back on",
    );
  } finally {
    harness.controls.dispose();
  }
});

test("turning Remote input back on restores the keyboard", () => {
  const harness = setup();
  try {
    harness.controlToggle.checked = false;
    harness.controlToggle.dispatch("change");
    harness.controlToggle.checked = true;
    harness.controlToggle.dispatch("change");

    assert.equal(harness.keyboardButton.disabled, false);
    assert.equal(harness.keyboardButton.title, "Type into the remote");
    harness.keyboardButton.dispatch("click");
    assert.equal(harness.controlToggle.checked, true);
    assert.equal(harness.keyboardButton.disabled, false);
    assert.equal(harness.calls.acquired, 2);
    assert.equal(harness.calls.focused, 1);
    assert.equal(harness.calls.textFocused, 1);
  } finally {
    harness.controls.dispose();
  }
});
