import assert from "node:assert/strict";
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
const inventory = await sprite.execFile(
  "bash",
  [
    "-lc",
    `
set -eu
for name in wf-recorder grim ffmpeg python3 timeout; do
  command -v "$name" || true
done
stat -c '%U %a %n' /tmp/sprite-desktop-rust-session/wayland-0
ffmpeg -hide_banner -h encoder=libvpx-vp9
ss -H -ltn 'sport = :3220'
sha256sum /home/sprite/rust-desktop/bin/sprite-desktop-gateway /home/sprite/rust-desktop/bin/sprite-desktop-streamd
`,
  ],
  { timeout: 15_000 },
);
assert.equal(inventory.exitCode, 0, inventory.stderr.toString());
console.log(inventory.stdout.toString());
console.log(inventory.stderr.toString());
console.log(
  JSON.stringify(
    {
      sprite: sprite.name,
      service: service.name,
      state: service.state?.status,
    },
    null,
    2,
  ),
);
