import assert from "node:assert/strict";
import { mkdir, writeFile } from "node:fs/promises";
import { dirname, resolve } from "node:path";
import { parseArgs } from "node:util";
import { assertDisposableTarget, loadTrialSprite } from "./trial.js";

const { values } = parseArgs({
  options: {
    sprite: { type: "string" },
    suite: { type: "string" },
    output: { type: "string" },
  },
  strict: true,
});
assert(values.sprite, "--sprite is required");
assert.equal(values.suite, "capture", "--suite must be capture");
assert(values.output, "--output is required");
assertDisposableTarget(values.sprite);
const output = resolve(values.output);
const sprite = await loadTrialSprite(values.sprite);
const remote = `/tmp/rust-native-capture-${process.pid}.png`;
try {
  const result = await sprite.execFile("grim", [remote], {
    env: {
      XDG_RUNTIME_DIR: "/tmp/sprite-desktop-rust-session",
      WAYLAND_DISPLAY: "wayland-0",
    },
  });
  assert.equal(result.exitCode, 0, result.stderr.toString());
  const image = await sprite.filesystem("/").readFile(remote);
  await mkdir(dirname(output), { recursive: true });
  await writeFile(output, image);
  console.log(
    JSON.stringify({ sprite: sprite.name, output, bytes: image.length }),
  );
} finally {
  await sprite.filesystem("/").rm(remote, { force: true });
}
