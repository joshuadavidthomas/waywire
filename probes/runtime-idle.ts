import assert from "node:assert/strict";
import { readFile, writeFile } from "node:fs/promises";
import { once } from "node:events";
import { setTimeout as sleep } from "node:timers/promises";
import { SpritesClient } from "@fly/sprites";
import WebSocket from "ws";
import { connectRFB } from "./rfb.js";

const token = process.env.SPRITES_TOKEN;
assert(token);
const client = new SpritesClient(token);
const sprite = await client.getSprite("sprite-desktop-v1-idle");
assert(
  sprite.url &&
    sprite.urlSettings?.auth === "sprite" &&
    sprite.urlSettings.privateAccess === "admins",
);
const fs = sprite.filesystem("/");
// libXss is a probe dependency, not a runtime dependency.
await sprite.execFile("sudo", [
  "apt-get",
  "install",
  "-y",
  "--no-install-recommends",
  "libxss1",
]);
await fs.writeFile(
  "/tmp/desktop-blanking.py",
  await readFile(new URL("runtime-saver.py", import.meta.url)),
);
const viewer = await connectRFB(sprite.url, token);
let pings = 0;
viewer.socket.on("ping", () => {
  pings++;
});
const started = Date.now();
console.log(
  `Thirty-minute runtime idle trial started at ${new Date(started).toISOString()}; no exec, HTTP polls, or RFB traffic during the wait.`,
);
try {
  await sleep(30 * 60_000 + 1000);
  assert.equal(viewer.socket.readyState, WebSocket.OPEN);
  assert(pings >= 89, `received only ${pings} pings`);
  const status = await fetch(sprite.url + "/healthz", {
    headers: { Authorization: `Bearer ${token}` },
    signal: AbortSignal.timeout(15_000),
  });
  assert.equal(status.status, 200);
  const health = (await status.json()) as {
    attached: number;
    task_held: boolean;
    release: string;
  };
  assert.equal(health.attached, 1);
  assert.equal(health.task_held, false);
  const saver = await sprite.execFile("python3", ["/tmp/desktop-blanking.py"]);
  const saverOutput = saver.stdout.toString();
  console.log(saverOutput);
  const closed = once(viewer.socket, "close");
  viewer.socket.close(1000);
  await closed;
  let idleMs: number | null = null;
  const detached = Date.now();
  for (let i = 0; i < 24; i++) {
    await sleep(5000);
    if ((await client.getSprite(sprite.name)).status !== "running") {
      idleMs = Date.now() - detached;
      break;
    }
  }
  assert(idleMs !== null, "Sprite did not idle within 120 seconds");
  const result = {
    sprite: sprite.name,
    release: health.release,
    started: new Date(started).toISOString(),
    durationMs: detached - started,
    pings,
    taskHeld: health.task_held,
    saverOutput,
    idleMs,
  };
  await writeFile(
    new URL("results/runtime-idle.json", import.meta.url),
    JSON.stringify(result, null, 2) + "\n",
  );
  console.log(JSON.stringify(result, null, 2));
} finally {
  viewer.socket.terminate();
}
