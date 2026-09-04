import assert from "node:assert/strict";
import { execFile } from "node:child_process";
import { writeFile } from "node:fs/promises";
import { setTimeout as sleep } from "node:timers/promises";
import { promisify } from "node:util";
import { SpritesClient } from "@fly/sprites";

const exec = promisify(execFile);
assert(process.env.SPRITES_TOKEN);
const client = new SpritesClient(process.env.SPRITES_TOKEN);
const sprite = await client.getSprite("sprite-desktop-v1");
const pageURL = "http://127.0.0.1:3217";
async function browser(...args: string[]) {
  const { stdout } = await exec(
    "agent-browser",
    ["--session", "sprite-v1-runtime", "--json", ...args],
    { timeout: 75_000 },
  );
  const response = JSON.parse(stdout);
  assert(response.success, JSON.stringify(response.error));
  return response.data;
}
async function connected() {
  await browser(
    "wait",
    "--fn",
    "document.querySelector('[data-first-frame-ms]') !== null && document.querySelector('[aria-label=\"Desktop controls\"] button')?.textContent?.includes('Disconnect')",
  );
}
async function timings() {
  const { result } = await browser(
    "eval",
    `({connected:performance.getEntriesByName('sprite-desktop-connected','mark').at(-1)?.startTime,frame:performance.getEntriesByName('sprite-desktop-first-frame','mark').at(-1)?.startTime})`,
  );
  assert(Number.isFinite(result.connected) && Number.isFinite(result.frame));
  return result as { connected: number; frame: number };
}
async function stop() {
  await browser(
    "eval",
    "[...document.querySelectorAll('button')].find(b=>b.textContent?.trim()==='Disconnect')?.click()",
  );
}
async function idle() {
  const started = Date.now();
  while (Date.now() - started < 120_000) {
    const current = await client.getSprite(sprite.name);
    if (current.status === "warm" || current.status === "cold")
      return { status: current.status, ms: Date.now() - started };
    await sleep(2000);
  }
  throw new Error("Sprite did not idle after Disconnect");
}
const warm = [],
  suspended = [],
  restarted = [],
  recoveries = [];
for (let trial = 0; trial < 5; trial++) {
  await browser("open", pageURL);
  await connected();
  warm.push(await timings());
}
for (const service of ["sprite-desktop", "sprite-desktop-bridge"]) {
  const before = (
    await browser(
      "eval",
      "performance.getEntriesByName('sprite-desktop-connected','mark').length",
    )
  ).result;
  const started = Date.now();
  await sprite.execFile("curl", [
    "--unix-socket",
    "/.sprite/api.sock",
    "-fsS",
    "-X",
    "POST",
    `http://sprite/v1/services/${service}/restart`,
  ]);
  await browser(
    "wait",
    "--fn",
    `performance.getEntriesByName('sprite-desktop-connected','mark').length > ${before}`,
  );
  await connected();
  const durationMs = Date.now() - started;
  assert(durationMs < 60_000);
  recoveries.push({ service, durationMs });
}
await browser("set", "offline", "on");
await browser("wait", "--text", "Browser offline");
await browser("set", "offline", "off");
await connected();
const health = await sprite.execFile("curl", [
  "-fsS",
  "http://127.0.0.1:8080/healthz",
]);
assert.equal(
  JSON.parse(health.stdout.toString()).attached,
  1,
  "offline/online created duplicate viewers",
);

for (let trial = 0; trial < 5; trial++) {
  await stop();
  const idleResult = await idle();
  await browser("open", pageURL);
  await connected();
  suspended.push({ ...(await timings()), idle: idleResult });
  console.log(`Suspended navigation ${trial + 1} passed`);
}
for (let trial = 0; trial < 3; trial++) {
  await stop();
  await client.restartSprite(sprite.name);
  const started = Date.now();
  let reloads = 0;
  for (;;) {
    await browser("open", pageURL);
    const { result: loaded } = await browser(
      "eval",
      "document.title === 'Sprite Desktop'",
    );
    if (loaded) break;
    reloads++;
    assert(
      Date.now() - started < 90_000,
      "Sprite URL never returned after machine restart",
    );
    await sleep(2000);
  }
  await connected();
  restarted.push({
    ...(await timings()),
    reloads,
    totalMs: Date.now() - started,
  });
  console.log(`Machine restart ${trial + 1} passed (${reloads} reloads)`);
}
await stop();
const stoppedRequests = (
  await browser("eval", "performance.getEntriesByType('resource').length")
).result;
const lastIdle = await idle();
assert.equal(
  (await browser("eval", "performance.getEntriesByType('resource').length"))
    .result,
  stoppedRequests,
);
const preferences = await sprite
  .filesystem("/")
  .readFile("/home/sprite/.config/xfce4/helpers.rc", "utf8");
assert.equal(
  preferences,
  "WebBrowser=firefox\nTerminalEmulator=xfce4-terminal\n",
);
assert.equal(
  await sprite
    .filesystem("/")
    .readFile("/home/sprite/v1-acceptance-persistence.txt", "utf8"),
  "keep this file across installation and restart\n",
);
const percentile = (values: number[], fraction: number) =>
  [...values].sort((a, b) => a - b)[Math.ceil(values.length * fraction) - 1];
const summary = Object.fromEntries(
  Object.entries({ warm, suspended, restarted }).map(([name, trials]) => [
    name,
    {
      connectedP50: percentile(
        trials.map((trial) => trial.connected),
        0.5,
      ),
      connectedP95: percentile(
        trials.map((trial) => trial.connected),
        0.95,
      ),
      frameP50: percentile(
        trials.map((trial) => trial.frame),
        0.5,
      ),
      frameP95: percentile(
        trials.map((trial) => trial.frame),
        0.95,
      ),
    },
  ]),
);
const result = {
  sprite: sprite.name,
  at: new Date().toISOString(),
  transport:
    "installed runtime through credential-isolating localhost acceptance proxy; not cookie-auth acceptance",
  warm,
  suspended,
  restarted,
  recoveries,
  offlineOnlineSingleViewer: true,
  stoppedWithoutRequests: true,
  lastIdle,
  persistence: true,
  summary,
};
await writeFile(
  new URL("results/runtime-browser-lifecycle.json", import.meta.url),
  JSON.stringify(result, null, 2) + "\n",
);
console.log(JSON.stringify(result, null, 2));
