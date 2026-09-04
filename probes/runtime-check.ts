import assert from "node:assert/strict";
import { randomBytes } from "node:crypto";
import { writeFile } from "node:fs/promises";
import { get } from "node:https";
import { setTimeout as sleep } from "node:timers/promises";
import { SpritesClient } from "@fly/sprites";
import WebSocket from "ws";
import { connectRFB } from "./rfb.js";

const token = process.env.SPRITES_TOKEN;
assert(token);
const sprite = await new SpritesClient(token).getSprite("sprite-desktop-v1");
assert(
  sprite.url &&
    sprite.urlSettings?.auth === "sprite" &&
    sprite.urlSettings.privateAccess === "admins",
);
const origin = sprite.url;
const headers = { Authorization: `Bearer ${token}` };
const results: unknown[] = [];
for (const sentOrigin of [
  undefined,
  "null",
  "%%%",
  origin.replace("https:", "http:"),
  "https://foreign.example.test",
  origin + ":443",
  [origin, origin],
  origin + ", " + origin,
]) {
  const status = await new Promise<number>((resolve, reject) => {
    const request = get(
      origin + "/vnc",
      {
        headers: {
          ...headers,
          Connection: "Upgrade",
          Upgrade: "websocket",
          "Sec-WebSocket-Version": "13",
          "Sec-WebSocket-Key": randomBytes(16).toString("base64"),
          ...(sentOrigin === undefined ? {} : { Origin: sentOrigin }),
        },
      },
      (response) => {
        response.resume();
        resolve(response.statusCode!);
      },
    );
    request.on("upgrade", (_response, socket) => {
      socket.destroy();
      reject(new Error("Invalid Origin upgraded"));
    });
    request.setTimeout(15_000, () =>
      request.destroy(new Error("Origin probe timed out")),
    );
    request.on("error", reject);
  });
  assert.equal(status, 403, JSON.stringify(sentOrigin));
  results.push({
    test: "Origin rejection",
    origin: sentOrigin ?? "missing",
    status,
  });
}
const root = await fetch(origin, {
  headers,
  signal: AbortSignal.timeout(15_000),
});
assert.equal(root.status, 200);
const html = await root.text();
const assets = [...html.matchAll(/(?:src|href)="(\/assets\/[^"\s]+)"/gu)].map(
  (match) => match[1]!,
);
assert(assets.length >= 2);
for (const path of ["/", "/healthz", "/version", ...assets]) {
  for (const method of ["GET", "HEAD"]) {
    const response = await fetch(origin + path, {
      method,
      headers,
      signal: AbortSignal.timeout(15_000),
    });
    assert.equal(response.status, 200, `${method} ${path}`);
    assert.equal(response.headers.get("x-content-type-options"), "nosniff");
    assert.equal(response.headers.get("referrer-policy"), "no-referrer");
    assert.match(
      response.headers.get("content-security-policy") ?? "",
      /frame-ancestors 'none'/u,
    );
    assert.equal(
      response.headers.get("cache-control"),
      path.startsWith("/assets/")
        ? "public, max-age=31536000, immutable"
        : "no-store",
    );
    const content = await response.text();
    if (method === "HEAD") assert.equal(content, "");
    assert(!content.includes(token), "API credential leaked into a response");
  }
}
for (const [method, path, status] of [
  ["POST", "/", 405],
  ["POST", "/healthz", 405],
  ["GET", "/vnc", 426],
  ["GET", "/missing", 404],
] as const) {
  const response = await fetch(origin + path, {
    method,
    headers,
    signal: AbortSignal.timeout(15_000),
  });
  assert.equal(response.status, status);
  await response.body?.cancel();
}
results.push({
  test: "installed route, cache, method, credential, and security-header contracts",
  assets,
  passed: true,
});

for (const attached of [true, false]) {
  const viewer: Awaited<ReturnType<typeof connectRFB>> | null = attached
    ? await connectRFB(origin, token)
    : null;
  try {
    const latencies: number[] = [];
    const started = performance.now();
    await Promise.all(
      Array.from({ length: 1000 }, async (_, index) => {
        await sleep(Math.max(0, started + index * 60 - performance.now()));
        const sent = performance.now();
        const response = await fetch(origin + "/healthz", {
          headers,
          signal: AbortSignal.timeout(15_000),
        });
        assert.equal(response.status, 200);
        const health = (await response.json()) as {
          rfb: string;
          attached: number;
        };
        assert.equal(health.rfb, "listening");
        if (!attached)
          assert.equal(
            health.attached,
            0,
            "close all browser viewers before the no-viewer trial",
          );
        latencies.push(performance.now() - sent);
      }),
    );
    if (viewer)
      assert.equal(
        viewer.socket.readyState,
        WebSocket.OPEN,
        "health polling disconnected the viewer",
      );
    const connectStart = performance.now();
    const fresh = await connectRFB(origin, token);
    const handshakeMs = performance.now() - connectStart;
    fresh.socket.close(1000);
    assert(handshakeMs < 2000, "health polling delayed a new RFB login");
    latencies.sort((a, b) => a - b);
    results.push({
      test: "1000 health requests per minute",
      attached,
      p95Ms: latencies[949],
      newHandshakeMs: handshakeMs,
      passed: true,
    });
    console.log(`Passed 1000/min health polling with attached=${attached}`);
  } finally {
    viewer?.socket.close(1000);
  }
  await sleep(200);
}
await writeFile(
  new URL("results/runtime-transport.json", import.meta.url),
  JSON.stringify(
    { sprite: sprite.name, at: new Date().toISOString(), results },
    null,
    2,
  ) + "\n",
);
console.log(JSON.stringify(results, null, 2));
