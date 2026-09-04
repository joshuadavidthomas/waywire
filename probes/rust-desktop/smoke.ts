// Live video checks only. This probe does not claim browser input or cursor behavior.
import assert from "node:assert/strict";
import { execFile } from "node:child_process";
import { once } from "node:events";
import { promisify } from "node:util";
import { createAcceptanceProxy } from "../view-runtime.js";
import { loadTrialSprite, requestedTrial } from "./trial.js";

const exec = promisify(execFile);
const { name, suite } = requestedTrial(["video"]);
const sprite = await loadTrialSprite(name);
const token = process.env.SPRITES_TOKEN!;
const proxy = createAcceptanceProxy(sprite.url!, token, "rust");
const session = `sprite-rust-${process.pid}`;

async function browser(...args: string[]) {
  const { stdout } = await exec(
    "agent-browser",
    ["--session", session, "--json", ...args],
    { timeout: 75_000 },
  );
  const reply = JSON.parse(stdout) as {
    success: boolean;
    data: unknown;
    error?: unknown;
  };
  assert(reply.success, JSON.stringify(reply.error));
  return reply.data;
}

async function evaluate<T>(expression: string): Promise<T> {
  const data = (await browser("eval", expression)) as { result: T };
  return data.result;
}

const connected =
  'document.querySelector("#status.connected") !== null && document.querySelector("#display").width >= 640 && document.querySelector("#display").height >= 360 && document.querySelector("#empty.hidden") !== null';
const dimensions =
  '(() => { const canvas = document.querySelector("#display"); return [canvas.width, canvas.height]; })()';
const pixelVariety = `(() => {
  const canvas = document.querySelector("#display");
  const context = canvas.getContext("2d");
  const width = Math.min(64, canvas.width);
  const height = Math.min(64, canvas.height);
  const pixels = context.getImageData(0, 0, width, height).data;
  const colors = new Set();
  for (let index = 0; index < pixels.length; index += 16) {
    colors.add(pixels[index] + "," + pixels[index + 1] + "," + pixels[index + 2] + "," + pixels[index + 3]);
  }
  return colors.size;
})()`;

proxy.server.listen(3217, "127.0.0.1");
await once(proxy.server, "listening");
try {
  await browser("open", "http://127.0.0.1:3217");
  assert.deepEqual(
    await evaluate<number[]>(
      "[WebSocket.CONNECTING, WebSocket.OPEN, WebSocket.CLOSING, WebSocket.CLOSED]",
    ),
    [0, 1, 2, 3],
    "browser automation changed native WebSocket constants",
  );
  await browser("wait", "--fn", connected);
  const firstDimensions = await evaluate<number[]>(dimensions);
  const firstColors = await evaluate<number>(pixelVariety);
  assert(firstColors > 8, `first viewer sample had only ${firstColors} colors`);

  await browser("tab", "new", "http://127.0.0.1:3217");
  await browser("wait", "--fn", connected);
  const secondDimensions = await evaluate<number[]>(dimensions);
  const secondColors = await evaluate<number>(pixelVariety);
  assert(
    secondColors > 8,
    `second viewer sample had only ${secondColors} colors`,
  );

  await browser("tab", "t1");
  await browser("wait", "--fn", connected);
  assert.deepEqual(
    await evaluate<number[]>(dimensions),
    secondDimensions,
    "viewers do not show the same current output size",
  );

  await browser("open", "http://127.0.0.1:3217");
  await browser("wait", "--fn", connected);
  const reconnectColors = await evaluate<number>(pixelVariety);
  assert(
    reconnectColors > 8,
    "static-desktop reconnect did not render real pixels",
  );

  console.log(
    JSON.stringify(
      {
        sprite: sprite.name,
        suite,
        firstDimensions,
        secondDimensions,
        firstColors,
        secondColors,
        reconnectColors,
        checks: ["first frame", "two viewers", "static desktop reconnect"],
        excluded: [
          "manual input",
          "application-visible browser input",
          "cursor shape and visibility",
          "physical latency",
        ],
        checkedAt: new Date().toISOString(),
      },
      null,
      2,
    ),
  );
} finally {
  await browser("close").catch(() => {});
  proxy.revokeCredential();
  proxy.server.closeAllConnections();
  if (proxy.server.listening) {
    await new Promise<void>((resolve) => proxy.server.close(() => resolve()));
  }
}
