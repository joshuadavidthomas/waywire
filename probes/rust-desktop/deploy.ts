// Trial installation requires a separate, literal opt-in.
import assert from "node:assert/strict";
import { execFile } from "node:child_process";
import { createHash, randomBytes } from "node:crypto";
import { readFile } from "node:fs/promises";
import { resolve } from "node:path";
import { parseArgs, promisify } from "node:util";
import {
  assertDisposableTarget,
  loadTrialSprite,
  sameTrialService,
  trialService,
  trialServiceName,
} from "./trial.js";

const execute = promisify(execFile);
const { values } = parseArgs({
  options: {
    sprite: { type: "string" },
    gateway: { type: "string" },
    streamd: { type: "string" },
    "authorize-deploy": { type: "boolean", default: false },
  },
  strict: true,
});
if (!values.sprite || !values.gateway || !values.streamd)
  throw new Error("--sprite, --gateway, and --streamd are required");
assertDisposableTarget(values.sprite);
if (!values["authorize-deploy"])
  throw new Error("deployment needs a separate --authorize-deploy opt-in");

const [gateway, streamd, launcher, revisionResult] = await Promise.all([
  readFile(resolve(values.gateway)),
  readFile(resolve(values.streamd)),
  readFile(new URL("run.sh", import.meta.url)),
  execute("jj", ["log", "-r", "@", "--no-graph", "-T", "commit_id"], {
    timeout: 10_000,
  }),
]);
for (const [name, bytes] of [
  ["gateway", gateway],
  ["streamd", streamd],
] as const) {
  assert(
    bytes.length > 1_000_000,
    `${name} does not look like a built Rust binary`,
  );
}
const revision = revisionResult.stdout.trim();
assert.match(revision, /^[0-9a-f]{40,64}$/u);
const owner = {
  schema: 1,
  owner: "dev.sprite-desktop.rust-trial",
  revision,
  gatewaySha256: createHash("sha256").update(gateway).digest("hex"),
  streamdSha256: createHash("sha256").update(streamd).digest("hex"),
};

const sprite = await loadTrialSprite(values.sprite);
const services = await sprite.listServices();
const protectedServices = new Set([
  "desktop",
  "sprite-desktop",
  "sprite-desktop-bridge",
  "waymote",
]);
const existingService = services.find(
  (service) => service.name === trialServiceName,
);
for (const service of services) {
  if (protectedServices.has(service.name))
    throw new Error(`refusing Sprite with protected service ${service.name}`);
  if (service.httpPort !== undefined && service.name !== trialServiceName)
    throw new Error(
      `foreign HTTP service owns the Sprite URL: ${service.name}`,
    );
}
if (existingService && !sameTrialService(existingService))
  throw new Error(
    `existing ${trialServiceName} service has an unowned definition`,
  );

const fs = sprite.filesystem("/");
const root = "/home/sprite/rust-desktop";
const marker = `${root}/trial-owner.json`;
const rootExists = await fs.exists(root);
const markerExists = await fs.exists(marker);
if (rootExists !== markerExists)
  throw new Error(
    "trial root and ownership marker do not agree; refusing mutation",
  );
if (markerExists) {
  const recorded = JSON.parse(await fs.readFile(marker, "utf8")) as {
    schema?: unknown;
    owner?: unknown;
  };
  if (recorded.schema !== 1 || recorded.owner !== owner.owner)
    throw new Error("trial root has an unowned ownership marker");
  const ownership = await sprite.execFile("stat", ["-c", "%U", root]);
  assert.equal(ownership.exitCode, 0, ownership.stderr.toString());
  assert.equal(
    ownership.stdout.toString().trim(),
    "sprite",
    "trial root is not sprite-owned",
  );
}
if (existingService && !markerExists)
  throw new Error("trial service exists without its ownership marker");

// This is the last read-only gate. No package installation or URL-policy update follows.
const prerequisites = await sprite.execFile("bash", [
  "-lc",
  'set -euo pipefail; for c in dbus-run-session ffmpeg flock jq labwc lxqt-session sprite-env ss wayland-info wlr-randr; do command -v "$c" >/dev/null; done; dpkg-query -W breeze-cursor-theme lxqt-wayland-session >/dev/null',
]);
assert.equal(prerequisites.exitCode, 0, prerequisites.stderr.toString());

const staging = `${root}/.upload-${randomBytes(8).toString("hex")}`;
await fs.mkdir(`${staging}/bin`, { recursive: true, mode: 0o700 });
try {
  await Promise.all([
    fs.writeFile(`${staging}/bin/sprite-desktop-gateway`, gateway, {
      mode: 0o700,
    }),
    fs.writeFile(`${staging}/bin/sprite-desktop-streamd`, streamd, {
      mode: 0o700,
    }),
    fs.writeFile(`${staging}/run.sh`, launcher, { mode: 0o700 }),
    fs.writeFile(
      `${staging}/trial-owner.json`,
      JSON.stringify(owner, null, 2) + "\n",
      { mode: 0o600 },
    ),
  ]);
  await fs.mkdir(`${root}/bin`, { recursive: true, mode: 0o700 });
  for (const path of [
    "bin/sprite-desktop-gateway",
    "bin/sprite-desktop-streamd",
    "run.sh",
    "trial-owner.json",
  ]) {
    await fs.rename(`${staging}/${path}`, `${root}/${path}`);
  }
} finally {
  await fs.rm(staging, { recursive: true, force: true });
}

// Replace only the definition checked above. The deployed Services API has
// DELETE/PUT; the SDK's newer /restart endpoint is not available there.
if (existingService) await sprite.deleteService(trialServiceName);
const logs = await sprite.createService(
  trialServiceName,
  {
    cmd: trialService.cmd,
    args: trialService.args,
    httpPort: trialService.httpPort,
    needs: trialService.needs,
    dir: trialService.dir,
  },
  "5s",
);
await logs.processAll((event) => {
  if (event.data) process.stderr.write(event.data);
});
console.log(`Deployed revision ${revision} to disposable ${sprite.name}.`);
