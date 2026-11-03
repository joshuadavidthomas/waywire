import assert from "node:assert/strict";
import test from "node:test";

import { WaywireSession } from "./session.ts";
import {
  deferredSocketOptions,
  FakeTarget,
  FakeWebSocket,
  flush,
  installBrowser,
  installGlobal,
  socket,
  type SocketAttempt,
  surfaceOptions,
} from "./test-support.ts";

async function resolveAttempts(
  attempts: SocketAttempt[],
): Promise<FakeWebSocket[]> {
  const sockets = attempts.map(() => new FakeWebSocket());
  attempts.forEach((attempt, index) => {
    const candidate = sockets[index];
    if (candidate) attempt.resolve(socket(candidate));
  });
  await flush();
  return sockets;
}

test("reconnect creates fresh sockets and closes late old sockets", async () => {
  installBrowser();
  const attempts: SocketAttempt[] = [];
  const session = new WaywireSession(deferredSocketOptions(attempts));
  session.attachSurface(surfaceOptions());

  session.connect();
  await Promise.resolve();
  assert.equal(attempts.length, 2);
  session.disconnect();
  session.connect();
  await Promise.resolve();
  assert.equal(attempts.length, 4);

  const stale = await resolveAttempts(attempts.slice(0, 2));
  assert.ok(stale.every(({ closeCalls }) => closeCalls === 1));
  const current = await resolveAttempts(attempts.slice(2));
  assert.ok(current.every(({ closeCalls }) => closeCalls === 0));

  await session.dispose();
  assert.ok(current.every(({ closeCalls }) => closeCalls === 1));
});

test("visibility change tears down and re-establishes both sockets", async () => {
  const { document } = installBrowser();
  const attempts: SocketAttempt[] = [];
  const session = new WaywireSession(deferredSocketOptions(attempts));
  session.attachSurface(surfaceOptions());
  session.connect();
  await Promise.resolve();
  assert.deepEqual(attempts.map(({ path }) => path).sort(), [
    "/control",
    "/stream",
  ]);

  document.hidden = true;
  document.dispatch("visibilitychange", {});
  document.hidden = false;
  document.dispatch("visibilitychange", {});
  await Promise.resolve();
  assert.equal(attempts.length, 4);

  const stale = await resolveAttempts(attempts.slice(0, 2));
  const current = await resolveAttempts(attempts.slice(2));
  assert.ok(stale.every(({ closeCalls }) => closeCalls === 1));
  assert.ok(current.every(({ closeCalls }) => closeCalls === 0));
  await session.dispose();
});

test("session disposal is terminal, idempotent, and disposes the surface", async () => {
  const { document } = installBrowser();
  const canvas = new FakeTarget();
  const textInput = new FakeTarget();
  const session = new WaywireSession();
  const surface = session.attachSurface(surfaceOptions(canvas, textInput));
  assert.ok(canvas.listenerCount > 0);
  assert.ok(textInput.listenerCount > 0);
  assert.ok(document.listenerCount > 0);

  const first = session.dispose();
  assert.equal(first, session.dispose());
  await first;
  assert.equal(canvas.listenerCount, 0);
  assert.equal(textInput.listenerCount, 0);
  assert.equal(document.listenerCount, 0);
  assert.throws(() => surface.focus(), /disposed/);
  assert.throws(() => session.connect(), /disposed/);
  assert.throws(() => session.attachSurface(surfaceOptions()), /disposed/);
  assert.throws(() => session.on("state", () => undefined), /disposed/);
  assert.throws(() => session.input.acquire(), /disposed/);
  session.disconnect();
});

/** Browser globals for the video worker path; `respond` answers its init. */
function installWorkerBrowser(respond: (worker: FakeTarget) => void): {
  workers: FakeTarget[];
  sockets: string[];
} {
  installBrowser();
  const workers: FakeTarget[] = [];
  const sockets: string[] = [];
  class FakeOffscreenCanvas {
    getContext() {
      return { drawImage() {} };
    }
  }
  installGlobal("OffscreenCanvas", FakeOffscreenCanvas);
  installGlobal(
    "HTMLCanvasElement",
    class {
      transferControlToOffscreen() {}
    },
  );
  installGlobal(
    "Worker",
    class extends FakeTarget {
      constructor() {
        super();
        workers.push(this);
      }
      postMessage(message: { type: string }) {
        if (message.type === "init") queueMicrotask(() => respond(this));
      }
      terminate() {}
    },
  );
  installGlobal(
    "WebSocket",
    class extends FakeWebSocket {
      constructor(url: URL) {
        super();
        sockets.push(new URL(url).pathname);
      }
    },
  );
  return { workers, sockets };
}

function workerCanvas(): FakeTarget & { transfers: number } {
  const canvas = Object.assign(new FakeTarget(), {
    transfers: 0,
    transferControlToOffscreen() {
      canvas.transfers += 1;
      return new (
        globalThis as unknown as { OffscreenCanvas: new () => object }
      ).OffscreenCanvas();
    },
  });
  return canvas;
}

test("where video runs in a worker, the session keeps the attached canvas for good", async (t) => {
  t.after(() => {
    for (const name of ["Worker", "OffscreenCanvas", "HTMLCanvasElement"])
      delete (globalThis as Record<string, unknown>)[name];
  });
  installWorkerBrowser(() => undefined);
  const canvas = workerCanvas();
  const session = new WaywireSession({
    endpoint: "https://desktop.example.com",
  });
  const surface = session.attachSurface(surfaceOptions(canvas));
  assert.equal(canvas.transfers, 1);
  surface.dispose();
  assert.throws(
    () => session.attachSurface(surfaceOptions(canvas)),
    /already attached.*attach a new canvas/,
  );
  const drawnOn = Object.assign(new FakeTarget(), {
    transferControlToOffscreen() {
      throw new Error("InvalidStateError");
    },
  });
  assert.throws(
    () => session.attachSurface(surfaceOptions(drawnOn)),
    /already has a rendering context/,
  );
  session.attachSurface(surfaceOptions(workerCanvas()));
  await session.dispose();
});

test("a video worker that lacks a feature leaves the video on the page", async (t) => {
  t.after(() => {
    for (const name of ["Worker", "OffscreenCanvas", "HTMLCanvasElement"])
      delete (globalThis as Record<string, unknown>)[name];
  });
  const { workers, sockets } = installWorkerBrowser((worker) =>
    worker.dispatch("message", {
      data: { type: "ready", supported: false, timeOrigin: 0 },
    }),
  );
  installGlobal("VideoDecoder", class {});
  const session = new WaywireSession({
    endpoint: "https://desktop.example.com",
  });
  session.attachSurface(surfaceOptions(workerCanvas()));
  session.connect();
  assert.equal(workers.length, 1);
  assert.deepEqual(sockets, ["/control"], "the worker would open /stream");
  await flush();
  assert.deepEqual(sockets, ["/control", "/stream"]);
  await session.dispose();
});
