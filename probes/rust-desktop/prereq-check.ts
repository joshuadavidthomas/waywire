import assert from "node:assert/strict";
import { parseArgs } from "node:util";
import {
  loadTrialSprite,
  sameTrialService,
  trialServiceName,
} from "./trial.js";

const { values } = parseArgs({
  options: { sprite: { type: "string" } },
  strict: true,
});
if (!values.sprite) throw new Error("--sprite is required");
const sprite = await loadTrialSprite(values.sprite);
const services = await sprite.listServices();
const protectedServices = new Set([
  "desktop",
  "sprite-desktop",
  "sprite-desktop-bridge",
  "waymote",
]);
for (const service of services) {
  if (protectedServices.has(service.name))
    throw new Error(`refusing Sprite with protected service ${service.name}`);
  if (service.httpPort !== undefined && service.name !== trialServiceName)
    throw new Error(
      `foreign HTTP service owns the Sprite URL: ${service.name}`,
    );
  if (service.name === trialServiceName && !sameTrialService(service))
    throw new Error(
      `existing ${trialServiceName} service has an unowned definition`,
    );
}

const check = await sprite.execFileHTTP(
  "bash",
  [
    "-lc",
    `set -euo pipefail
. /etc/os-release
[ "\${ID:-}" = ubuntu ]
[ "$(id -un)" = sprite ]
[ "$(dpkg --print-architecture)" = amd64 ]
for command in dbus-run-session ffmpeg flock jq labwc lxqt-session sprite-env ss wayland-info wlr-randr; do
  command -v "$command" >/dev/null
done
dpkg-query -W -f='\${binary:Package}\t\${Version}\\n' \
  breeze-cursor-theme dbus-x11 ffmpeg labwc lxqt-core lxqt-wayland-session wayland-utils wlr-randr`,
  ],
  { timeout: 20_000 },
);
assert.equal(check.exitCode, 0, check.stderr.toString());
console.log(check.stdout.toString().trim());
console.log(
  `Prerequisites exist on ${sprite.name}; no package, file, URL policy, or service was changed.`,
);
