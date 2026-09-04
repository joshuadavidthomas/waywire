// Browser-state checks only. Desktop input is exercised with cua-driver.
import assert from "node:assert/strict";
import { execFile } from "node:child_process";
import { once } from "node:events";
import { writeFile } from "node:fs/promises";
import { setTimeout as sleep } from "node:timers/promises";
import { promisify } from "node:util";
import { SpritesClient } from "@fly/sprites";
import { createAcceptanceProxy } from "./view-runtime.js";

const exec = promisify(execFile);
const token = process.env.SPRITES_TOKEN;
assert(token);
const release = process.argv[2];
assert(release, "Pass the locally built upgrade release");
const sprite = await new SpritesClient(token).getSprite(
  "sprite-desktop-v1-conflicts",
);
const proxy = createAcceptanceProxy(sprite.url!, token);
proxy.server.listen(3217, "127.0.0.1");
await once(proxy.server, "listening");
const session = "sprite-v1-final";
async function browser(...args: string[]) {
  const { stdout } = await exec(
    "agent-browser",
    ["--session", session, "--json", ...args],
    { timeout: 75_000 },
  );
  const response = JSON.parse(stdout);
  assert(response.success, JSON.stringify(response.error));
  return response.data;
}
async function evaluate(expression: string) {
  return (await browser("eval", expression)).result;
}
async function waitFor(expression: string, budget = 60_000) {
  const started = Date.now();
  while (Date.now() - started < budget) {
    if (await evaluate(expression)) return Date.now() - started;
    await sleep(500);
  }
  throw new Error(
    `Browser condition missed its ${budget}ms budget: ${expression}`,
  );
}
const isConnected = `document.querySelector('[data-first-frame-ms]') !== null && [...document.querySelectorAll('button')].some(b => b.textContent.trim() === 'Disconnect')`;
const connections = `performance.getEntriesByName('sprite-desktop-connected','mark').length`;
const dimensions = `(() => { const c = document.querySelector('canvas'); return [c.width, c.height]; })()`;
const pixels = `Array.from(document.querySelector('canvas').getContext('2d').getImageData(50,100,40,40).data)`;
const results: Record<string, unknown> = {
  sprite: sprite.name,
  release,
  started: new Date().toISOString(),
  transport:
    "credential-isolating localhost proxy; user separately confirmed real cookie access",
};
try {
  await browser("open", "http://127.0.0.1:3217");
  await waitFor(isConnected);
  const originalSize = await evaluate(dimensions);
  await browser("tab", "new", "http://127.0.0.1:3217");
  await waitFor(isConnected);
  await browser("set", "viewport", "1280", "800");
  await sleep(2000);
  const secondSize = await evaluate(dimensions);
  const secondPixels = await evaluate(pixels);
  await browser("tab", "t1");
  await waitFor(isConnected);
  assert.deepEqual(
    await evaluate(dimensions),
    secondSize,
    "tabs do not share the remote resolution",
  );
  assert.notDeepEqual(
    secondSize,
    originalSize,
    "second tab did not resize the shared display",
  );
  const firstPixels: number[] = await evaluate(pixels);
  // VNC's JPEG encoding is lossy; separate clients need not decode identical bytes.
  const meanPixelDifference =
    firstPixels.reduce(
      (sum, value, index) => sum + Math.abs(value - secondPixels[index]),
      0,
    ) / firstPixels.length;
  assert(new Set(firstPixels).size > 20, "sample contains no desktop detail");
  assert(
    meanPixelDifference < 5,
    `tabs differ by ${meanPixelDifference} intensity levels on average`,
  );
  const health = await sprite.execFile("curl", [
    "-fsS",
    "http://127.0.0.1:8080/healthz",
  ]);
  assert.equal(JSON.parse(health.stdout.toString()).attached, 2);
  results.sharedTabs = {
    dimensions: secondSize,
    meanPixelDifference,
    attached: 2,
  };
  await browser("tab", "close", "t2");
  await evaluate(
    `window.hiddenAt = 0; window.returnedAt = 0; document.addEventListener('visibilitychange', () => { if(document.hidden) window.hiddenAt = performance.now(); else window.returnedAt = performance.now(); }); true`,
  );
  await browser("tab", "new", "about:blank");
  console.log(
    `Hidden-tab wait started at ${new Date().toISOString()}; no desktop actions during the next 20 minutes`,
  );
  await sleep(20 * 60_000 + 1000);
  await browser("tab", "t1");
  const returnRecoveryMs = await waitFor(isConnected);
  const hiddenMs = await evaluate("window.returnedAt - window.hiddenAt");
  assert(
    hiddenMs >= 20 * 60_000,
    `tab was not actually hidden for 20 minutes: ${hiddenMs}`,
  );
  results.hiddenTab = { hiddenMs, returnRecoveryMs };
  console.log("Shared tabs and hidden-tab return passed");

  const before = await evaluate(connections);
  const upgraded = await exec(
    "pnpm",
    ["install-desktop", "--sprite", sprite.name, "--release", release],
    { timeout: 240_000 },
  );
  assert(
    (upgraded.stdout + upgraded.stderr).includes(
      "may end an active desktop session",
    ),
  );
  await waitFor(`${connections} > ${before} && (${isConnected})`);
  const version = await evaluate("fetch('/version').then(r => r.json())");
  assert.equal(version.release, release);
  results.activeUpgrade = {
    before,
    after: await evaluate(connections),
    release: version.release,
  };
  console.log("Active upgrade and reconnect passed");

  const revoked = Date.now();
  proxy.revokeCredential();
  await waitFor(
    `document.body.textContent.includes('Could not connect') && [...document.querySelectorAll('button')].some(b => b.textContent.trim() === 'Re-authenticate')`,
    75_000,
  );
  results.authFailure = { failedAfterMs: Date.now() - revoked };
  const resourceCount = await evaluate(
    "performance.getEntriesByType('resource').length",
  );
  await sleep(3000);
  assert.equal(
    await evaluate("performance.getEntriesByType('resource').length"),
    resourceCount,
    "failed viewer kept polling",
  );
  await evaluate(
    `[...document.querySelectorAll('button')].find(b => b.textContent.trim() === 'Re-authenticate').click()`,
  );
  await waitFor("location.hostname === 'sprites.dev'", 30_000);
  results.reauthenticationReachedFly = true;
  await writeFile(
    new URL("results/runtime-browser-acceptance.json", import.meta.url),
    JSON.stringify(results, null, 2) + "\n",
  );
  console.log(JSON.stringify(results, null, 2));
} finally {
  await browser("close").catch(() => {});
  proxy.revokeCredential();
  proxy.server.close();
  proxy.server.closeAllConnections();
}
