// Disposable M0 acceptance checks. Never use this against a Sprite containing user work.
import assert from "node:assert/strict";
import { writeFile } from "node:fs/promises";
import { SpritesClient } from "@fly/sprites";

const token = process.env.SPRITES_TOKEN;
assert(token, "SPRITES_TOKEN is required");
const name = "sprite-desktop-v1-probe";
const client = new SpritesClient(token);
let sprite = await client.getSprite(name);
assert.equal(sprite.urlSettings?.auth, "sprite");
assert.equal(sprite.urlSettings?.privateAccess, "admins");
await sprite.updateURLSettings({ auth: "sprite", privateAccess: "admins" });
sprite = await client.getSprite(name);
assert.deepEqual(sprite.urlSettings, {
  auth: "sprite",
  privateAccess: "admins",
});
const inside = await sprite.execFile("sprite-env", ["info"]);
const environment = JSON.parse(inside.stdout.toString());
assert.equal(environment.sprite_url, sprite.url);
const sessions = await sprite.listSessions();
assert.equal(sessions.length, 0);
const origin = new URL(sprite.url!).origin;
const headers = { Authorization: `Bearer ${token}` };
const results: Record<string, unknown> = {
  sprite: name,
  origin,
  checkedAt: new Date().toISOString(),
  sdk: "@fly/sprites 0.2.2 read/update/read URLSettings passed",
  spriteEnv: "sprite_url matches getSprite().url",
};

const trials = [];
for (let trial = 1; trial <= 3; trial++) {
  const before = await fetch(origin + "/healthz", {
    headers,
    signal: AbortSignal.timeout(15_000),
  });
  assert.equal(before.status, 200);
  const previous = await before.json();
  assert(typeof previous === "object" && previous !== null);
  assert("started_at" in previous && typeof previous.started_at === "string");
  assert("connections_active" in previous);
  assert.equal(
    previous.connections_active,
    0,
    "Do not restart while another probe viewer is attached",
  );
  const began = Date.now();
  await sprite.restart();
  const restartReturnedMs = Date.now() - began;
  const requests = [];
  let recovered = false;
  while (Date.now() - began < 60_000) {
    const requestBegan = Date.now();
    try {
      const response = await fetch(origin + "/healthz", {
        headers,
        signal: AbortSignal.timeout(12_000),
      });
      const body = await response.text();
      requests.push({
        status: response.status,
        elapsedMs: Date.now() - requestBegan,
      });
      if (response.ok) {
        const current: unknown = JSON.parse(body);
        assert(typeof current === "object" && current !== null);
        assert(
          "started_at" in current && typeof current.started_at === "string",
        );
        if (current.started_at !== previous.started_at) {
          recovered = true;
          break;
        }
      }
    } catch (error) {
      requests.push({
        error: error instanceof Error ? error.name : String(error),
        elapsedMs: Date.now() - requestBegan,
      });
    }
    await new Promise((resolve) => setTimeout(resolve, 1000));
  }
  const result = {
    trial,
    restartReturnedMs,
    recovered,
    totalMs: Date.now() - began,
    requests,
  };
  trials.push(result);
  console.log(JSON.stringify(result));
  assert(recovered, "Probe did not recover within 60s after machine restart");
}
results.restarts = trials;
await writeFile(
  "/tmp/sprite-desktop-m0-sdk.json",
  JSON.stringify(results, null, 2) + "\n",
);
console.log(JSON.stringify(results, null, 2));
