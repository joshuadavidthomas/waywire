import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import {
  loadTrialSprite,
  sameTrialService,
  trialServiceName,
} from "../rust-desktop/trial.js";

const sprite = await loadTrialSprite("sprite-desktop-rust");
const service = (await sprite.listServices()).find(
  (entry) => entry.name === trialServiceName,
);
assert(
  service && sameTrialService(service),
  "owned Rust desktop service is required",
);
const stun = await sprite.execFile(
  "python3",
  ["-c", await readFile(new URL("stun-check.py", import.meta.url), "utf8")],
  { timeout: 6000 },
);
console.log(stun.stdout.toString());
assert.equal(
  stun.exitCode,
  0,
  "Sprite UDP STUN check failed; do not prepare a capture path yet",
);
const install = await sprite.execFile(
  "sudo",
  ["-n", "apt-get", "install", "-y", "--no-install-recommends", "wf-recorder"],
  { timeout: 90_000 },
);
console.log(install.stdout.toString());
console.log(install.stderr.toString());
assert.equal(install.exitCode, 0, "capture helper installation failed");
const help = await sprite.execFile("wf-recorder", ["--help"], {
  timeout: 5000,
});
console.log(help.stdout.toString());
console.log(help.stderr.toString());
assert.equal(help.exitCode, 0);
