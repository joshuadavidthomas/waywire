import { parseArgs } from "node:util";
import { readFile } from "node:fs/promises";
import { resolve } from "node:path";

import { APIError, SpritesClient, type Sprite } from "@fly/sprites";

const { values } = parseArgs({
  options: {
    name: { type: "string", default: "josh-desktop" },
  },
  strict: true,
});

const name = values.name;
if (!name || !/^[a-z0-9][a-z0-9-]{0,62}$/u.test(name)) {
  throw new Error(
    "--name must contain 1–63 lowercase letters, digits, or hyphens",
  );
}

const token = process.env.SPRITES_TOKEN;
if (!token) throw new Error("SPRITES_TOKEN is required");

const scriptsDirectory = resolve(import.meta.dirname, "../../../sprite");
const [provisionScript, desktopScript] = await Promise.all([
  readFile(resolve(scriptsDirectory, "provision.sh"), "utf8"),
  readFile(resolve(scriptsDirectory, "desktop.sh"), "utf8"),
]);

const client = new SpritesClient(token);
let sprite: Sprite;

try {
  sprite = await client.getSprite(name);
  console.log(`Using existing sprite ${name}`);
} catch (error) {
  if (!(error instanceof APIError) || error.statusCode !== 404) throw error;
  console.log(`Creating sprite ${name}`);
  sprite = await client.createSprite(name, {
    urlSettings: { auth: "sprite" },
  });
}

const filesystem = sprite.filesystem("/home/sprite");
await Promise.all([
  filesystem.writeFile("provision.sh", provisionScript, { mode: 0o755 }),
  filesystem.writeFile("desktop.sh", desktopScript, { mode: 0o755 }),
]);

console.log("Installing the desktop packages. This can take several minutes.");
const provision = sprite.spawn("bash", ["/home/sprite/provision.sh"]);
provision.stdout.pipe(process.stdout, { end: false });
provision.stderr.pipe(process.stderr, { end: false });
const provisionExitCode = await provision.wait();
if (provisionExitCode !== 0) {
  throw new Error(`Provisioning exited with code ${provisionExitCode}`);
}

const serviceCommand = "/home/sprite/.local/bin/desktop.sh";
const existingService = (await sprite.listServices()).find(
  (service) => service.name === "desktop",
);
const serviceMatches =
  existingService?.cmd === serviceCommand && existingService.args.length === 0;

if (existingService && !serviceMatches) {
  console.log("Replacing the obsolete desktop service definition");
  await sprite.deleteService("desktop");
}

if (serviceMatches) {
  await sprite.execFile("sprite-env", ["services", "restart", "desktop"]);
} else {
  await sprite.execFile("sprite-env", [
    "services",
    "create",
    "desktop",
    "--cmd",
    serviceCommand,
  ]);
}

let listening = false;
let sockets = "";
for (let attempt = 0; attempt < 20; attempt += 1) {
  const result = await sprite.execFile("ss", ["-ltnp", "sport = :5900"]);
  sockets = result.stdout.toString();
  if (sockets.includes("LISTEN")) {
    listening = true;
    break;
  }
  await new Promise((resolveDelay) => setTimeout(resolveDelay, 500));
}

if (!listening) {
  throw new Error(`Desktop service did not listen on :5900.\n${sockets}`);
}

const healthResponse = await fetch(
  `https://api.sprites.dev/v1/sprites/${encodeURIComponent(name)}/check`,
  {
    headers: { Authorization: `Bearer ${token}` },
  },
);
const health = healthResponse.ok
  ? await healthResponse.text()
  : `HTTP ${healthResponse.status}`;

console.log(`\nSprite ${name} is ready.`);
console.log("VNC is listening on localhost:5900.");
console.log(`Health: ${health}`);
