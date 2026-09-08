import assert from "node:assert/strict";
import test from "node:test";

import { Effect } from "effect";
import { acquireWaymoteSession } from "./effect.ts";
import type { SurfaceOptions, WaymoteSession } from "./waymote.ts";

class FakeTarget {
  readonly listeners = new Map<string, Set<(event: unknown) => void>>();
  readonly style = {
    cursor: "",
    getPropertyValue: (): string => this.style.cursor,
    getPropertyPriority: (): string => "",
    setProperty: (_name: string, value: string): void => {
      this.style.cursor = value;
    },
    removeProperty: (): void => {
      this.style.cursor = "";
    },
  };
  addEventListener(type: string, listener: (event: unknown) => void): void {
    let listeners = this.listeners.get(type);
    if (!listeners) this.listeners.set(type, (listeners = new Set()));
    listeners.add(listener);
  }
  removeEventListener(type: string, listener: (event: unknown) => void): void {
    this.listeners.get(type)?.delete(listener);
  }
  get listenerCount(): number {
    return [...this.listeners.values()].reduce(
      (count, listeners) => count + listeners.size,
      0,
    );
  }
}

function installGlobal(name: string, value: unknown): void {
  Object.defineProperty(globalThis, name, {
    configurable: true,
    writable: true,
    value,
  });
}

function surfaceOptions(
  canvas: FakeTarget,
  textInput: FakeTarget,
): SurfaceOptions {
  if (!("getContext" in canvas) || !("focus" in textInput))
    throw new TypeError("incomplete DOM fake");
  return {
    canvas: canvas as unknown as HTMLCanvasElement,
    textInputElement: textInput as unknown as HTMLInputElement,
  };
}

function fixture(
  windowValue: object = Object.assign(new FakeTarget(), {
    devicePixelRatio: 1,
  }),
) {
  const fakeDocument = Object.assign(new FakeTarget(), {
    hidden: false,
    pointerLockElement: null,
  });
  installGlobal("window", windowValue);
  installGlobal("document", fakeDocument);
  const canvas = Object.assign(new FakeTarget(), {
    width: 1280,
    height: 720,
    getContext: () => ({}),
    focus() {},
  });
  const textInput = Object.assign(new FakeTarget(), { value: "", focus() {} });
  return { fakeDocument, canvas, textInput };
}

test("the Effect scope disposes the session surface and its browser listeners", async () => {
  const { fakeDocument, canvas, textInput } = fixture();
  let acquiredSession: WaymoteSession | undefined;
  await Effect.runPromise(
    Effect.scoped(
      Effect.map(
        acquireWaymoteSession({}, surfaceOptions(canvas, textInput)),
        ({ session }) => {
          acquiredSession = session;
          assert.ok(canvas.listenerCount > 0);
          assert.ok(textInput.listenerCount > 0);
          assert.ok(fakeDocument.listenerCount > 0);
        },
      ),
    ),
  );
  assert.equal(canvas.listenerCount, 0);
  assert.equal(textInput.listenerCount, 0);
  assert.equal(fakeDocument.listenerCount, 0);
  const disposedSession = acquiredSession;
  if (!disposedSession) throw new Error("session was not acquired");
  assert.throws(() => disposedSession.connect(), /disposed/);
});

test("acquisition cleans up when connect fails after the surface attaches", async () => {
  const windowTarget = Object.assign(new FakeTarget(), { devicePixelRatio: 1 });
  const failingWindow = new Proxy(windowTarget, {
    has(target, property) {
      if (property === "VideoDecoder")
        throw new Error("WebCodecs probe failed");
      return Reflect.has(target, property);
    },
  });
  const { fakeDocument, canvas, textInput } = fixture(failingWindow);
  await assert.rejects(
    Effect.runPromise(
      Effect.scoped(
        acquireWaymoteSession({}, surfaceOptions(canvas, textInput)),
      ),
    ),
    /WebCodecs probe failed/,
  );
  assert.equal(canvas.listenerCount, 0);
  assert.equal(textInput.listenerCount, 0);
  assert.equal(fakeDocument.listenerCount, 0);
  assert.equal(windowTarget.listenerCount, 0);
});
