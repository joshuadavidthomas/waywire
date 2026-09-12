import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import test from "node:test";

import type {
  QualityPreset,
  RemoteDisplayPolicy,
  SurfaceHandle,
  WaywireSession,
  WaywireStats,
} from "../sdk/waywire.ts";
import {
  installViewerListeners,
  QUALITY_STORAGE_KEY,
  QUALITY_VALUES,
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

class FakeOption extends FakeElement {
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
    HTMLOptionElement: { configurable: true, value: FakeOption },
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
  readonly resolutionStatusOption: FakeOption;
  readonly qualityLabel: FakeElement;
  readonly qualitySelect: FakeSelect;
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
  const qualityLabel = element();
  panel.queryAllResults.set("button, label", [
    protectedLabel,
    resolutionLabel,
    qualityLabel,
  ]);
  const resolutionSelect = new FakeSelect();
  const resolutionStatusOption = new FakeOption();
  resolutionStatusOption.disabled = true;
  resolutionStatusOption.textContent = "Waiting for video";
  const qualitySelect = new FakeSelect();
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
      resolutionStatusOption,
      resolutionLabel,
      qualitySelect,
      qualityLabel,
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
    resolutionStatusOption,
    qualityLabel,
    qualitySelect,
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
  readonly resolutionStatusOption: FakeOption;
  readonly qualityLabel: FakeElement;
  readonly qualitySelect: FakeSelect;
  readonly storage: FakeStorage;
  readonly emitStats: (stats: WaywireStats) => void;
  readonly calls: {
    acquired: number;
    released: number;
    focused: number;
    textFocused: number;
    policies: RemoteDisplayPolicy[];
    qualities: QualityPreset[];
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
    qualities: [] as QualityPreset[],
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
      setQuality: (quality: QualityPreset) => calls.qualities.push(quality),
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
    resolutionStatusOption: created.resolutionStatusOption,
    qualityLabel: created.qualityLabel,
    qualitySelect: created.qualitySelect,
    storage,
    emitStats: (stats) => {
      for (const listener of statsListeners) listener(stats);
    },
    calls,
  };
}

function stats(width: number, height: number): WaywireStats {
  return {
    width,
    height,
    renderedFps: 60,
    renderedMediaTimestampMicros: 0,
    drawCompletedAtMs: 0,
    generation: 1,
    bitrateKbps: 8000,
    scalePercent: 100,
    rttMs: 20,
    clockConfident: false,
    clockUncertaintyMs: null,
    latencyTargetMs: 100,
    latenessMs: 0,
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
}

test("playout adapts through the SDK setter using one wall clock with the HUD hidden", (context) => {
  let now = 0;
  context.mock.method(performance, "now", () => now);
  const harness = setup();
  const sample = { ...stats(1280, 720), clockConfident: true, latenessMs: 30 };
  try {
    assert.equal(harness.elements.hud.hidden, true);
    for (let frame = 0; frame < 3; frame += 1) {
      now = frame * 16;
      harness.emitStats(sample);
    }
    assert.deepEqual(harness.calls.targets, []);
    now = 48;
    harness.emitStats(sample);
    assert.deepEqual(harness.calls.targets, [125]);
    now = 10_047;
    harness.emitStats({ ...sample, clockConfident: false });
    assert.deepEqual(harness.calls.targets, [125]);
    now = 10_048;
    harness.emitStats({ ...sample, latenessMs: 0 });
    assert.deepEqual(harness.calls.targets, [125, 100]);
    assert.equal(harness.storage.getItem("waywire.latency"), null);
    harness.controls.dispose();
    now = 20_000;
    for (let frame = 0; frame < 4; frame += 1) harness.emitStats(sample);
    assert.deepEqual(harness.calls.targets, [125, 100]);
  } finally {
    harness.controls.dispose();
  }
});

test("the viewer lowers the real target while completely idle and cancels ticks on disposal", (context) => {
  let now = 0;
  context.mock.method(performance, "now", () => now);
  context.mock.timers.enable({ apis: ["setInterval"] });
  const harness = setup();
  const tick = (): void => {
    now += 1_000;
    context.mock.timers.tick(1_000);
  };
  try {
    for (let frame = 0; frame < 4; frame += 1)
      harness.emitStats({
        ...stats(1280, 720),
        clockConfident: true,
        latenessMs: 30,
      });
    assert.deepEqual(harness.calls.targets, [125]);
    assert.equal(harness.elements.hud.hidden, true);
    for (let second = 1; second < 10; second += 1) tick();
    assert.deepEqual(harness.calls.targets, [125]);
    tick();
    assert.deepEqual(harness.calls.targets, [125, 100]);
    for (let second = 0; second < 5; second += 1) tick();
    assert.deepEqual(harness.calls.targets, [125, 100, 75]);
    const targetField = harness.elements.hud.querySelector(
      '[data-field="video-latency-target"]',
    );
    assert.equal(
      targetField?.textContent,
      "75 ms",
      "the idle HUD must not show the stale frame target",
    );
    // Opening the HUD must likewise render the current target, not the last frame's snapshot.
    harness.elements.hudToggle.checked = true;
    (harness.elements.hudToggle as unknown as FakeElement).dispatch("change");
    assert.equal(targetField?.textContent, "75 ms");
    harness.controls.dispose();
    for (let second = 0; second < 10; second += 1) tick();
    assert.deepEqual(harness.calls.targets, [125, 100, 75]);
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
  assert.match(
    select,
    /<option id="resolution-status" value="" selected disabled>[\s\S]*?Waiting for video[\s\S]*?<\/option>/,
  );
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

test("quality options use the exact value order and display labels", () => {
  const html = readFileSync(
    new URL("../../index.html", import.meta.url),
    "utf8",
  );
  const select = html.match(
    /<select[\s\S]*?id="quality"[\s\S]*?>([\s\S]*?)<\/select>/,
  )?.[1];
  assert.ok(select, "quality select was not found");
  const options = [
    ...select.matchAll(/<option value="([^"]+)">([^<]+)<\/option>/g),
  ].map((match) => [match[1], match[2]]);

  assert.deepEqual(
    options.map(([value]) => value),
    QUALITY_VALUES,
  );
  assert.deepEqual(
    options.map(([, label]) => label),
    ["Automatic", "High", "Medium", "Low"],
  );
  assert.ok(
    html.indexOf('id="resolution"') < html.indexOf('id="quality"'),
    "quality must follow resolution",
  );
});

test("viewer favicon links the exact monitor glyph and ink", () => {
  const html = readFileSync(
    new URL("../../index.html", import.meta.url),
    "utf8",
  );
  assert.match(
    html,
    /<link rel="icon" type="image\/svg\+xml" href="\/favicon\.svg" \/>/,
  );
  const favicon = readFileSync(
    new URL("../../public/favicon.svg", import.meta.url),
    "utf8",
  );
  assert.equal(
    favicon,
    '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="#ededed" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">\n' +
      '  <rect width="20" height="14" x="2" y="3" rx="2" />\n' +
      '  <path d="M8 21h8" />\n' +
      '  <path d="M12 17v4" />\n' +
      "</svg>\n",
  );
});

test("stored fixed resolution is restored before viewer actions", () => {
  const harness = setup({ [RESOLUTION_STORAGE_KEY]: "1920x1080" });
  try {
    assert.equal(harness.resolutionSelect.value, "");
    assert.equal(
      harness.resolutionStatusOption.textContent,
      "Waiting for video",
    );
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

test("stored quality is restored through the SDK setter", () => {
  const harness = setup({ [QUALITY_STORAGE_KEY]: "medium" });
  try {
    assert.equal(harness.qualitySelect.value, "medium");
    assert.deepEqual(harness.calls.qualities, ["medium"]);
  } finally {
    harness.controls.dispose();
  }
});

test("quality changes persist and reject values outside the exact list", () => {
  const harness = setup();
  try {
    assert.equal(harness.qualitySelect.value, "automatic");
    assert.deepEqual(harness.calls.qualities, ["automatic"]);

    harness.qualitySelect.value = "high";
    harness.qualitySelect.dispatch("change");
    assert.equal(harness.storage.getItem(QUALITY_STORAGE_KEY), "high");
    assert.deepEqual(harness.calls.qualities, ["automatic", "high"]);
    assert.equal(harness.calls.acquired, 1);
    assert.equal(harness.calls.focused, 0, "selection keeps native focus");

    harness.qualitySelect.value = "ultra";
    harness.qualitySelect.dispatch("change");
    assert.equal(harness.qualitySelect.value, "high");
    assert.equal(harness.storage.getItem(QUALITY_STORAGE_KEY), "high");
    assert.deepEqual(harness.calls.qualities, ["automatic", "high"]);
    assert.equal(harness.calls.acquired, 1);
  } finally {
    harness.controls.dispose();
  }
});

test("stored quality outside the exact list falls back to Automatic", () => {
  const harness = setup({ [QUALITY_STORAGE_KEY]: "HIGH" });
  try {
    assert.equal(harness.qualitySelect.value, "automatic");
    assert.deepEqual(harness.calls.qualities, ["automatic"]);
    assert.equal(harness.storage.getItem(QUALITY_STORAGE_KEY), "automatic");
  } finally {
    harness.controls.dispose();
  }
});

test("resolution changes set exact fixed and Fit policies", () => {
  const harness = setup();
  try {
    assert.equal(harness.resolutionSelect.value, "");
    assert.deepEqual(harness.calls.policies, []);
    assert.equal(harness.storage.getItem(RESOLUTION_STORAGE_KEY), null);

    harness.resolutionSelect.value = "2560x1440";
    harness.resolutionSelect.dispatch("change");
    assert.deepEqual(harness.calls.policies, [
      {
        mode: "fixed",
        width: 2560,
        height: 1440,
        scale: 1,
      },
    ]);
    assert.equal(harness.resolutionSelect.value, "");
    assert.equal(harness.storage.getItem(RESOLUTION_STORAGE_KEY), "2560x1440");

    harness.resolutionSelect.value = "fit";
    harness.resolutionSelect.dispatch("change");
    assert.deepEqual(harness.calls.policies.at(-1), {
      mode: "observe",
      element: harness.elements.display,
      devicePixelRatio: 1,
    });
    assert.equal(harness.resolutionSelect.value, "");
    assert.equal(harness.storage.getItem(RESOLUTION_STORAGE_KEY), "fit");
    assert.equal(
      harness.calls.acquired,
      0,
      "resolution changes do not acquire input ownership",
    );
    assert.equal(
      harness.calls.focused,
      0,
      "selection keeps native keyboard focus",
    );

    harness.resolutionSelect.value = "800x600";
    harness.resolutionSelect.dispatch("change");
    assert.equal(harness.resolutionSelect.value, "");
    assert.equal(harness.calls.policies.length, 2);
    assert.equal(harness.storage.getItem(RESOLUTION_STORAGE_KEY), "fit");
  } finally {
    harness.controls.dispose();
  }
});

test("absent and invalid stored resolutions leave the remote display alone", () => {
  const fresh = setup();
  const invalid = setup({ [RESOLUTION_STORAGE_KEY]: "1920 × 1080" });
  try {
    assert.equal(fresh.resolutionSelect.value, "");
    assert.deepEqual(fresh.calls.policies, []);
    assert.equal(fresh.storage.getItem(RESOLUTION_STORAGE_KEY), null);

    assert.equal(invalid.resolutionSelect.value, "");
    assert.deepEqual(invalid.calls.policies, []);
    assert.equal(
      invalid.storage.getItem(RESOLUTION_STORAGE_KEY),
      "1920 × 1080",
    );
  } finally {
    fresh.controls.dispose();
    invalid.controls.dispose();
  }
});

test("stored Fit is restored while the menu waits for live video", () => {
  const harness = setup({ [RESOLUTION_STORAGE_KEY]: "fit" });
  try {
    assert.equal(harness.resolutionSelect.value, "");
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

test("live frame dimensions drive the resolution display without changing its policy", () => {
  const harness = setup();
  try {
    harness.emitStats(stats(0, 0));
    assert.equal(harness.resolutionSelect.value, "");
    assert.equal(
      harness.resolutionStatusOption.textContent,
      "Waiting for video",
    );
    harness.emitStats(stats(1920, 1080));
    assert.equal(harness.resolutionSelect.value, "1920x1080");
    assert.equal(harness.resolutionStatusOption.hidden, true);
    assert.deepEqual(harness.calls.policies, []);
    assert.equal(harness.storage.getItem(RESOLUTION_STORAGE_KEY), null);

    const dynamicOption = harness.resolutionStatusOption;
    harness.emitStats(stats(1200, 674));
    assert.equal(harness.resolutionSelect.value, "1200x674");
    assert.equal(dynamicOption.hidden, false);
    assert.equal(dynamicOption.value, "1200x674");
    assert.equal(dynamicOption.textContent, "1200 × 674");

    harness.emitStats(stats(100, 90));
    harness.emitStats(stats(100, 90));
    assert.equal(harness.resolutionStatusOption, dynamicOption);
    assert.equal(dynamicOption.value, "100x90");
    assert.equal(dynamicOption.textContent, "100 × 90");
    assert.deepEqual(harness.calls.policies, []);
    assert.equal(harness.storage.getItem(RESOLUTION_STORAGE_KEY), null);
  } finally {
    harness.controls.dispose();
  }
});

test("unchanged frame dimensions do not interrupt native resolution navigation", () => {
  const harness = setup();
  try {
    harness.emitStats(stats(1920, 1080));
    // The user is navigating the native popup but has not committed a change.
    harness.resolutionSelect.value = "1280x720";
    harness.emitStats(stats(1920, 1080));
    assert.equal(harness.resolutionSelect.value, "1280x720");
    assert.deepEqual(harness.calls.policies, []);
    harness.resolutionSelect.dispatch("change");
    assert.equal(harness.calls.policies.length, 1);
    assert.equal(harness.resolutionSelect.value, "1920x1080");
  } finally {
    harness.controls.dispose();
  }
});

test("a fixed action fires once and later frames only update the live display", () => {
  const harness = setup();
  try {
    harness.emitStats(stats(1200, 674));
    harness.resolutionSelect.value = "1600x900";
    harness.resolutionSelect.dispatch("change");
    assert.deepEqual(harness.calls.policies, [
      { mode: "fixed", width: 1600, height: 900, scale: 1 },
    ]);
    assert.equal(harness.storage.getItem(RESOLUTION_STORAGE_KEY), "1600x900");
    assert.equal(
      harness.resolutionSelect.value,
      "1200x674",
      "the known stream stays visible while the requested mode is pending",
    );

    harness.emitStats(stats(800, 450));
    assert.equal(harness.resolutionSelect.value, "800x450");
    assert.equal(harness.calls.policies.length, 1);
    assert.equal(harness.storage.getItem(RESOLUTION_STORAGE_KEY), "1600x900");
  } finally {
    harness.controls.dispose();
  }
});

test("select labels keep their native pointer action", () => {
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
    assert.equal(
      harness.qualityLabel.dispatch("pointerdown").defaultPrevented,
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
    harness.qualitySelect.value = "low";
    harness.qualitySelect.dispatch("change");
    assert.deepEqual(harness.calls.qualities, ["automatic", "low"]);
    assert.equal(harness.storage.getItem(QUALITY_STORAGE_KEY), "low");
    assert.equal(harness.controlToggle.checked, false);
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
