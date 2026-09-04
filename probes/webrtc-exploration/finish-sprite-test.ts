import assert from "node:assert/strict";
import { mkdir, writeFile } from "node:fs/promises";
import {
  loadTrialSprite,
  sameTrialService,
  trialServiceName,
} from "../rust-desktop/trial.js";

const sprite = await loadTrialSprite("sprite-desktop-rust");
const service = (await sprite.listServices()).find(
  (entry) => entry.name === trialServiceName,
);
assert(service && sameTrialService(service));
const fs = sprite.filesystem("/");
const destination =
  "probes/webrtc-exploration/results/sprite-network-2026-09-07";
await mkdir(destination, { recursive: true });
const roots = [1788792684300, 1788792804803, 1788793023915, 1788793548108];
let firstOwner:
  | {
      originalOutput: {
        name: string;
        modes: {
          width: number;
          height: number;
          refresh: number;
          current: boolean;
        }[];
      }[];
    }
  | undefined;
for (const stamp of roots) {
  const root = `/home/sprite/webrtc-test-${stamp}`;
  const owner = await fs.readFile(`${root}/owner.json`);
  const log = await fs.readFile(`${root}/server.log`);
  if (!firstOwner)
    firstOwner = JSON.parse(owner.toString()) as typeof firstOwner;
  await writeFile(`${destination}/${stamp}-owner.json`, owner);
  await writeFile(`${destination}/${stamp}-server.log`, log);
}
const output = firstOwner?.originalOutput.find(
  (entry) => entry.name === "HEADLESS-1",
);
const originalMode = output?.modes.find((mode) => mode.current);
assert(originalMode, "original output mode was not saved");
const restore = await sprite.execFile(
  "env",
  [
    "XDG_RUNTIME_DIR=/tmp/sprite-desktop-rust-session",
    "WAYLAND_DISPLAY=wayland-0",
    "wlr-randr",
    "--output",
    "HEADLESS-1",
    "--custom-mode",
    `${originalMode.width}x${originalMode.height}@${originalMode.refresh}`,
  ],
  { timeout: 5000 },
);
assert.equal(restore.exitCode, 0, restore.stderr.toString());
const check = await sprite.execFile(
  "bash",
  [
    "-lc",
    "sha256sum /home/sprite/rust-desktop/bin/sprite-desktop-gateway /home/sprite/rust-desktop/bin/sprite-desktop-streamd; ss -H -ltn 'sport = :3220'",
  ],
  { timeout: 5000 },
);
assert.equal(check.exitCode, 0, check.stderr.toString());
const lines = check.stdout.toString().trim().split("\n");
assert.equal(lines.length, 2, "test port remains open");
assert(
  lines[0]?.startsWith(
    "0c771df4c0fb48d99971efc3133b52a7b3f0dd0474812bd8cd3ba72ddaa6abb4 ",
  ),
);
assert(
  lines[1]?.startsWith(
    "3341878c62241739569ec6b9b0e435afea1fba12050da5914040f5bca92bab14 ",
  ),
);
const report = {
  status: "BLOCKED_NO_DIRECT_ICE_ROUTE",
  sprite: sprite.name,
  service: service.state?.status,
  udpStunBindingSucceededAtBothEnds: true,
  browserIceResponsesReceived: 0,
  desktopCaptureStarted: false,
  playbackObserved: false,
  installedWlrPairUnchanged: true,
  spriteTestPortEmpty: true,
  originalModeRestored: originalMode,
  packageInstalled: "wf-recorder 0.6.0-1build1",
  turnRelayProvisioned: false,
  attempts: roots,
};
await writeFile(`${destination}/summary.json`, JSON.stringify(report, null, 2));
console.log(JSON.stringify(report, null, 2));
