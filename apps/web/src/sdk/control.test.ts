import assert from "node:assert/strict";
import test from "node:test";

import { WaywireSession } from "./session.ts";
import {
  FakeTarget,
  installBrowser,
  installGlobal,
  surfaceOptions,
} from "./test-support.ts";

test("double-click stays unlocked until pointer lock is requested", async () => {
  installBrowser();
  let lockRequests = 0;
  const canvas = new FakeTarget();
  canvas.requestPointerLock = async () => {
    lockRequests += 1;
  };
  const session = new WaywireSession();
  const surface = session.attachSurface(surfaceOptions(canvas));
  try {
    canvas.dispatch("dblclick", {});
    assert.equal(lockRequests, 0);
    await surface.requestPointerLock();
    assert.equal(lockRequests, 1);
  } finally {
    await session.dispose();
  }
});

test("dispose cancels an asynchronous clipboard paste", async () => {
  installBrowser();
  let resolveClipboard: ((text: string) => void) | undefined;
  const clipboardText = new Promise<string>((resolve) => {
    resolveClipboard = resolve;
  });
  installGlobal("navigator", {
    clipboard: { readText: () => clipboardText },
  });
  const canvas = new FakeTarget();
  const session = new WaywireSession();
  session.attachSurface(surfaceOptions(canvas));
  const originalWarn = console.warn;
  const warnings: unknown[][] = [];
  console.warn = (...values: unknown[]) => warnings.push(values);
  try {
    canvas.dispatch("keydown", {
      code: "KeyV",
      ctrlKey: true,
      metaKey: false,
      shiftKey: false,
      repeat: false,
      isComposing: false,
      keyCode: 86,
      preventDefault() {},
      stopPropagation() {},
    });
    const disposal = session.dispose();
    resolveClipboard?.("text after disposal");
    await disposal;
    await clipboardText;
    await Promise.resolve();
    assert.deepEqual(warnings, []);
  } finally {
    console.warn = originalWarn;
  }
});
