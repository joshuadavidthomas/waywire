import assert from "node:assert/strict";
import { randomBytes } from "node:crypto";
import { readFile } from "node:fs/promises";
import { get } from "node:https";
import { resolve } from "node:path";
import { parseArgs } from "node:util";
import { SpritesClient } from "@fly/sprites";
import { valid } from "semver";

const { values } = parseArgs({
  options: {
    sprite: { type: "string" },
    release: { type: "string" },
  },
  strict: true,
});
const name = values.sprite;
const release = values.release;
if (!name || !/^[a-z0-9][a-z0-9-]{0,62}$/u.test(name))
  throw new Error("--sprite must name an existing Sprite");
if (
  !release ||
  !/^v\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?$/u.test(release) ||
  !valid(release)
)
  throw new Error("--release must name a built version, such as v1.0.0");
const token = process.env.SPRITES_TOKEN;
if (!token)
  throw new Error("SPRITES_TOKEN is required on the operator's machine");

const client = new SpritesClient(token);
const sprite = await client.getSprite(name);
if (sprite.urlSettings?.auth !== "sprite")
  throw new Error(
    `Set URL authentication first: sprite url update --auth sprite -s ${name}`,
  );
if (sprite.urlSettings.privateAccess !== "admins")
  throw new Error(
    `Set private access to admins in the Sprites dashboard for ${name}`,
  );
if (!sprite.url) throw new Error("Sprite has no canonical URL");
const origin = new URL(sprite.url).origin;
const bundle = resolve(import.meta.dirname, "../../../dist/releases", release);
const archiveName = `waywire-${release}-linux-amd64.tar.gz`;
const [installer, archive] = await Promise.all([
  readFile(resolve(bundle, "install.sh")),
  readFile(resolve(bundle, archiveName)),
]);
console.log(
  "The in-Sprite preflight checks package sources before changing apt.",
);
const remote = `/tmp/waywire-upload-${randomBytes(8).toString("hex")}`;
const fs = sprite.filesystem("/");
await fs.mkdir(remote, { recursive: true });
try {
  await fs.writeFile(`${remote}/install.sh`, installer, { mode: 0o700 });
  await fs.writeFile(`${remote}/${archiveName}`, archive, { mode: 0o600 });
  const command = sprite.spawn("bash", [
    `${remote}/install.sh`,
    "--archive",
    `${remote}/${archiveName}`,
  ]);
  command.stdout.pipe(process.stdout, { end: false });
  command.stderr.pipe(process.stderr, { end: false });
  const code = await command.wait();
  if (code !== 0)
    throw new Error(
      `Installer exited ${code}; inspect its output before provisioning again`,
    );
} finally {
  await fs.rm(remote, { recursive: true, force: true });
}

const services = await sprite.listServices();
const service = services.find((candidate) => candidate.name === "waywire");
assert(service, "waywire service is missing");
assert.equal(service.state?.status, "running");
assert.equal(service.httpPort, 8080);
const record = JSON.parse(
  await fs.readFile("/var/lib/waywire/install.json", "utf8"),
) as {
  state: string;
  release: string;
  services: unknown[];
};
assert.equal(record.state, "committed");
assert.equal(record.release, release);
assert.equal(record.services.length, 1);
const health = await sprite.check();
assert.equal(health.status, "healthy");
const response = await fetch(origin + "/healthz", {
  headers: { Authorization: `Bearer ${token}` },
  signal: AbortSignal.timeout(15_000),
});
assert.equal(response.status, 200);
assert.equal(await response.text(), "ok\n");
const unauthenticated = await fetch(origin, {
  redirect: "manual",
  signal: AbortSignal.timeout(15_000),
});
assert.equal(unauthenticated.status, 302);
assert.equal(
  new URL(unauthenticated.headers.get("location")!).hostname,
  "sprites.dev",
);
await unauthenticated.body?.cancel();
for (const path of ["/stream", "/control"]) {
  await new Promise<void>((resolveCheck, reject) => {
    const request = get(
      origin + path,
      {
        headers: {
          Connection: "Upgrade",
          Upgrade: "websocket",
          "Sec-WebSocket-Version": "13",
          "Sec-WebSocket-Key": randomBytes(16).toString("base64"),
        },
      },
      (result) => {
        result.resume();
        try {
          assert.equal(
            result.statusCode,
            302,
            `Unauthenticated ${path} did not require login`,
          );
          assert.equal(
            new URL(result.headers.location!).hostname,
            "sprites.dev",
          );
          resolveCheck();
        } catch (error) {
          reject(error);
        }
      },
    );
    request.setTimeout(15_000, () =>
      request.destroy(new Error(`Private ${path} check timed out`)),
    );
    request.on("upgrade", (_response, socket) => {
      socket.destroy();
      reject(new Error(`Unauthenticated ${path} WebSocket was accepted`));
    });
    request.on("error", reject);
  });
}
console.log(`Installed ${release}. Open ${origin}`);
