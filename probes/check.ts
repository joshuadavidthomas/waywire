import assert from "node:assert/strict";
import { randomBytes } from "node:crypto";
import { writeFile } from "node:fs/promises";
import { get } from "node:https";
import { parseArgs } from "node:util";
import WebSocket from "ws";

const { values } = parseArgs({
  options: {
    sprite: { type: "string", default: "sprite-desktop-v1-probe" },
    seconds: { type: "string", default: "300" },
    "require-task": { type: "boolean", default: false },
    report: { type: "string", default: "/tmp/sprite-desktop-m0-check.json" },
  },
});
const token = process.env.SPRITES_TOKEN;
assert(token, "SPRITES_TOKEN is required");
const seconds = Number(values.seconds);
assert(Number.isInteger(seconds) && seconds >= 0 && seconds <= 3600);
const headers = { Authorization: `Bearer ${token}` };
const api = `https://api.sprites.dev/v1/sprites/${encodeURIComponent(values.sprite!)}`;
const infoResponse = await fetch(api, { headers });
assert.equal(infoResponse.status, 200);
const info = await infoResponse.json();
assert.equal(info.url_settings.auth, "sprite");
assert.equal(info.url_settings.private_access, "admins");
const origin = new URL(info.url).origin;
const results: Record<string, unknown> = {
  sprite: values.sprite,
  origin,
  started: new Date().toISOString(),
};
const wsHeaders = {
  Connection: "Upgrade",
  Upgrade: "websocket",
  "Sec-WebSocket-Version": "13",
  "Sec-WebSocket-Key": randomBytes(16).toString("base64"),
};

function rejectedUpgrade(requestHeaders: Record<string, string>) {
  return new Promise<{ status: number; location?: string }>(
    (resolve, reject) => {
      const request = get(
        origin + "/echo",
        { headers: { ...wsHeaders, ...requestHeaders } },
        (response) => {
          response.resume();
          resolve({
            status: response.statusCode!,
            location: response.headers.location,
          });
        },
      );
      request.setTimeout(15_000, () =>
        request.destroy(new Error("Upgrade timed out")),
      );
      request.on("upgrade", (_response, socket) => {
        socket.destroy();
        reject(new Error("Forbidden upgrade was accepted"));
      });
      request.on("error", reject);
    },
  );
}
// Probe only the URL during the soak. No exec, console, health or API polling.
const unauthenticatedPage = await fetch(origin, {
  redirect: "manual",
  signal: AbortSignal.timeout(15_000),
});
assert.equal(unauthenticatedPage.status, 302);
assert.equal(
  new URL(unauthenticatedPage.headers.get("location")!).hostname,
  "sprites.dev",
);
await unauthenticatedPage.body?.cancel();
const unauthenticatedSocket = await rejectedUpgrade({});
assert.equal(unauthenticatedSocket.status, 302);
assert.equal(new URL(unauthenticatedSocket.location!).hostname, "sprites.dev");
results.unauthenticated = "HTTP and WebSocket redirected to sprites.dev";
for (const badOrigin of [
  undefined,
  "null",
  "https://other.example",
  "http://" + new URL(origin).host,
  origin + ":443",
  origin + ", https://other.example",
]) {
  const response = await rejectedUpgrade({
    ...headers,
    ...(badOrigin ? { Origin: badOrigin } : {}),
  });
  assert.equal(response.status, 403, `Origin ${badOrigin}: ${response.status}`);
}
results.invalidOrigins = "six cases rejected with 403";
console.log(
  "URL policy, unauthenticated redirects, and Origin rejection passed.",
);

const payload = randomBytes(1024 * 1024);
const text = "sprite-desktop-m0";
let pings = 0;
let textEcho = false;
let binaryEcho = false;
let lastPing: string | undefined;
let elapsed = 0;
let closeCode = 0;
let closeTimedOut = false;
const openedAt = Date.now();
await new Promise<void>((resolve, reject) => {
  const socket = new WebSocket(origin.replace("https:", "wss:") + "/echo", {
    headers: { ...headers, Origin: origin },
    handshakeTimeout: 15_000,
  });
  let timer: ReturnType<typeof setTimeout>;
  let closing = false;
  let finalEcho = false;
  const timeout = setTimeout(
    () => fail(new Error("Initial echo timed out")),
    20_000,
  );
  function fail(error: Error) {
    clearTimeout(timeout);
    clearTimeout(timer);
    socket.terminate();
    reject(error);
  }
  socket.on("error", fail);
  socket.on("open", () => {
    socket.send(text);
    socket.send(payload);
  });
  socket.on("ping", () => {
    pings++;
    lastPing = new Date().toISOString();
    if (pings % 15 === 0)
      console.log(`Idle soak: ${pings} server pings received`);
  });
  socket.on("message", (data, binary) => {
    try {
      if (finalEcho) {
        assert(!binary);
        assert.equal(data.toString(), "after-idle");
        closing = true;
        elapsed = Date.now() - openedAt;
        clearTimeout(timer);
        timer = setTimeout(() => {
          closeTimedOut = true;
          socket.terminate();
        }, 10_000);
        socket.close(1000, "M0 complete");
        return;
      }
      if (binary) {
        assert.deepEqual(data, payload);
        binaryEcho = true;
      } else {
        assert.equal(data.toString(), text);
        textEcho = true;
      }
      if (textEcho && binaryEcho) {
        clearTimeout(timeout);
        console.log(
          `Text and 1 MiB binary echo passed. Holding idle for ${seconds}s.`,
        );
        timer = setTimeout(async () => {
          try {
            if (values["require-task"]) {
              const response = await fetch(origin + "/healthz", {
                headers,
                signal: AbortSignal.timeout(5000),
              });
              assert.equal(response.status, 200);
              const health = await response.json();
              assert.equal(health.task_held, true);
              results.taskWhileAttached = health.task_held;
            }
            finalEcho = true;
            socket.send("after-idle");
            timer = setTimeout(
              () => fail(new Error("Post-idle echo timed out")),
              15_000,
            );
          } catch (error) {
            fail(error as Error);
          }
        }, seconds * 1000);
      }
    } catch (error) {
      fail(error as Error);
    }
  });
  socket.on("close", (code) => {
    clearTimeout(timeout);
    clearTimeout(timer);
    closeCode = code;
    if (closing) resolve();
    else reject(new Error(`WebSocket dropped during idle: ${code}`));
  });
});
results.echo = {
  textEcho,
  binaryEcho,
  idleSeconds: seconds,
  elapsedMs: elapsed,
  pings,
  lastPing,
  closeCode,
  closeTimedOut,
};
const healthResponse = await fetch(origin + "/healthz", {
  headers,
  signal: AbortSignal.timeout(15_000),
});
assert.equal(healthResponse.status, 200);
results.health = await healthResponse.json();
results.finished = new Date().toISOString();
await writeFile(values.report!, JSON.stringify(results, null, 2) + "\n");
console.log(JSON.stringify(results, null, 2));
