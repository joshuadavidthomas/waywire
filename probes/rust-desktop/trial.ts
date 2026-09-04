import { parseArgs } from "node:util";
import {
  SpritesClient,
  type ServiceWithState,
  type Sprite,
} from "@fly/sprites";

export const trialSpriteName = "sprite-desktop-rust";
export const trialServiceName = "rust-desktop";

const protectedSprites = new Set([
  "josh-desktop",
  "sprite-desktop-v1",
  "sprite-desktop-v1-conflicts",
  "sprite-desktop-waymote",
]);

export const trialService = {
  name: trialServiceName,
  cmd: "/usr/bin/dbus-run-session",
  args: ["--", "bash", "/home/sprite/rust-desktop/run.sh"],
  httpPort: 8080,
  needs: [] as string[],
  dir: "/home/sprite",
};

export function requestedTrial(suites: readonly string[]) {
  const { values } = parseArgs({
    options: {
      sprite: { type: "string" },
      suite: { type: "string", default: "video" },
    },
    strict: true,
  });
  if (!values.sprite) throw new Error("--sprite is required");
  assertDisposableTarget(values.sprite);
  if (!suites.includes(values.suite))
    throw new Error(`--suite must be one of: ${suites.join(", ")}`);
  return { name: values.sprite, suite: values.suite };
}

export function assertDisposableTarget(name: string): void {
  if (protectedSprites.has(name))
    throw new Error(`refusing protected Sprite: ${name}`);
  if (name !== trialSpriteName)
    throw new Error(`this trial may target only ${trialSpriteName}`);
}

export async function loadTrialSprite(name: string): Promise<Sprite> {
  assertDisposableTarget(name);
  const token = process.env.SPRITES_TOKEN;
  if (!token)
    throw new Error("SPRITES_TOKEN is required on the operator machine");
  const sprite = await new SpritesClient(token).getSprite(name);
  if (sprite.urlSettings?.auth !== "sprite")
    throw new Error(`${name} must have auth=sprite before any trial operation`);
  if (sprite.urlSettings.privateAccess !== "admins")
    throw new Error(
      `${name} must have private_access=admins before any trial operation`,
    );
  if (!sprite.url) throw new Error(`${name} has no canonical URL`);
  return sprite;
}

export function sameTrialService(service: ServiceWithState): boolean {
  return (
    service.name === trialService.name &&
    service.cmd === trialService.cmd &&
    JSON.stringify(service.args ?? []) === JSON.stringify(trialService.args) &&
    service.httpPort === trialService.httpPort &&
    JSON.stringify(service.needs ?? []) ===
      JSON.stringify(trialService.needs) &&
    service.dir === trialService.dir &&
    Object.keys(service.env ?? {}).length === 0
  );
}
