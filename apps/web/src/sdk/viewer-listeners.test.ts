import assert from "node:assert/strict";
import test from "node:test";

import type { WaymoteEventMap, WaymoteStats } from "./waymote.ts";
import {
  installViewerListeners,
  type ViewerElements,
} from "./viewer-listeners.ts";

class FakeElement extends EventTarget {
  textContent = "";
  value = "";
  disabled = false;
  readonly classes = new Set<string>();
  readonly classList = {
    add: (name: string) => this.classes.add(name),
    remove: (name: string) => this.classes.delete(name),
    toggle: (name: string, force?: boolean) => {
      const add = force ?? !this.classes.has(name);
      if (add) this.classes.add(name);
      else this.classes.delete(name);
      return add;
    },
  };
}

function stats(width: number): WaymoteStats {
  return {
    width,
    height: 720,
    renderedFps: 60,
    renderedMediaTimestampMicros: width * 1000,
    drawCompletedAtMs: width,
    generation: 1,
    bitrateKbps: 4000,
    scalePercent: 100,
    rttMs: 2,
    clockConfident: true,
    clockUncertaintyMs: 1,
    latencyTargetMs: 60,
    latenessMs: 3,
    pendingInputCount: 0,
    decoderQueue: 0,
    receivedFrames: width,
    decodedFrames: width,
    presentedFrames: width,
    droppedFrames: 0,
    overdueDroppedFrames: 0,
    decodedOverflowDroppedFrames: 0,
    decoderResetDroppedFrames: 0,
    decoderResets: 1,
    resizeState: "idle",
  };
}

test("viewer listeners render latest metrics at 4 Hz and clean up", () => {
  let now = 0;
  let nextTimer = 1;
  const timers = new Map<number, () => void>();
  Object.defineProperty(globalThis, "performance", {
    configurable: true,
    value: { now: () => now },
  });
  Object.defineProperty(globalThis, "setTimeout", {
    configurable: true,
    value: (callback: () => void) => {
      const handle = nextTimer++;
      timers.set(handle, callback);
      return handle;
    },
  });
  Object.defineProperty(globalThis, "clearTimeout", {
    configurable: true,
    value: (handle: number) => timers.delete(handle),
  });

  const buttons = [new FakeElement(), new FakeElement()];
  Object.defineProperty(globalThis, "document", {
    configurable: true,
    value: { querySelectorAll: () => buttons },
  });
  const element = () => new FakeElement();
  const rawElements = {
    display: element(),
    empty: element(),
    status: element(),
    controlStatus: element(),
    codec: element(),
    metrics: element(),
    latency: element(),
    pointerLockButton: element(),
    textInputButton: element(),
    sendClipboardButton: element(),
    copyClipboardButton: element(),
    clipboardStatus: element(),
    imeProxy: element(),
  };
  rawElements.latency.value = "90";
  const elements = rawElements as unknown as ViewerElements;
  const listeners = new Map<
    keyof WaymoteEventMap,
    Set<(value: never) => void>
  >();
  let latency = 0;
  let acquireCalls = 0;
  const session = {
    on(type: keyof WaymoteEventMap, listener: (value: never) => void) {
      let group = listeners.get(type);
      if (!group) listeners.set(type, (group = new Set()));
      group.add(listener);
      return () => group?.delete(listener);
    },
    video: { setLatencyTarget: (value: number) => (latency = value) },
    input: { acquire: () => acquireCalls++ },
  };
  const emit = <K extends keyof WaymoteEventMap>(
    type: K,
    value: WaymoteEventMap[K],
  ) => {
    for (const listener of listeners.get(type) ?? []) listener(value as never);
  };
  let focusCalls = 0;
  const remove = installViewerListeners(
    elements,
    session as unknown as Parameters<typeof installViewerListeners>[1],
    { focusTextInput: () => focusCalls++ } as unknown as Parameters<
      typeof installViewerListeners
    >[2],
  );

  emit("stats", stats(1));
  now = 10;
  emit("stats", stats(2));
  now = 100;
  emit("stats", stats(3));
  assert.match(rawElements.metrics.textContent, /^1×720/);
  assert.equal(timers.size, 1);

  now = 250;
  timers.values().next().value?.();
  timers.clear();
  assert.match(rawElements.metrics.textContent, /^3×720/);

  rawElements.latency.dispatchEvent(new Event("change"));
  rawElements.textInputButton.dispatchEvent(new Event("click"));
  assert.equal(latency, 90);
  assert.equal(acquireCalls, 1);
  assert.equal(focusCalls, 1);

  now = 260;
  emit("stats", stats(4));
  assert.equal(timers.size, 1);
  remove();
  assert.equal(timers.size, 0);
  assert.ok([...listeners.values()].every((group) => group.size === 0));
  rawElements.latency.value = "120";
  rawElements.latency.dispatchEvent(new Event("change"));
  rawElements.textInputButton.dispatchEvent(new Event("click"));
  assert.equal(latency, 90);
  assert.equal(acquireCalls, 1);
  buttons[0]?.dispatchEvent(new Event("pointerdown", { cancelable: true }));
  assert.equal(
    buttons[0]?.dispatchEvent(new Event("pointerdown", { cancelable: true })),
    true,
  );
});
