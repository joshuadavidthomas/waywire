import assert from "node:assert/strict";
import test from "node:test";

import { WaymoteSession } from "./session.ts";
import {
  deferredSocketOptions,
  FakeTarget,
  FakeWebSocket,
  flush,
  installBrowser,
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
  const session = new WaymoteSession(deferredSocketOptions(attempts));
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
  const session = new WaymoteSession(deferredSocketOptions(attempts));
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
  const session = new WaymoteSession();
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
