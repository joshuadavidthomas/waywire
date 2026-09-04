import assert from "node:assert/strict";
import {
  loadTrialSprite,
  requestedTrial,
  sameTrialService,
  trialServiceName,
} from "./trial.js";

const { name, suite } = requestedTrial(["video"]);
const sprite = await loadTrialSprite(name);
const service = (await sprite.listServices()).find(
  (candidate) => candidate.name === trialServiceName,
);
assert(service, `${trialServiceName} service is missing`);
assert(
  sameTrialService(service),
  `${trialServiceName} has an unowned definition`,
);
assert.equal(
  service.state?.status,
  "running",
  `${trialServiceName} is not running`,
);
assert(service.state.pid && service.state.pid > 1, "service has no live PID");

const health = await sprite.execFile("curl", [
  "-fsS",
  "--max-time",
  "5",
  "http://127.0.0.1:8080/healthz",
]);
assert.equal(health.exitCode, 0, health.stderr.toString());
assert.equal(health.stdout.toString(), "ok\n");

const processResult = await sprite.execFile("ps", ["-eo", "pid=,ppid=,args="]);
assert.equal(processResult.exitCode, 0, processResult.stderr.toString());
const processes = processResult.stdout
  .toString()
  .trim()
  .split("\n")
  .map((line) => {
    const match = /^\s*(\d+)\s+(\d+)\s+(.+)$/u.exec(line);
    assert(match, `could not parse process row: ${line}`);
    return {
      pid: Number(match[1]),
      parent: Number(match[2]),
      command: match[3]!,
    };
  });
const one = (needle: string) => {
  const matches = processes.filter(
    (process) =>
      process.command === needle || process.command.startsWith(`${needle} `),
  );
  assert.equal(
    matches.length,
    1,
    `expected one ${needle} process, saw ${matches.length}`,
  );
  return matches[0]!;
};
const gateway = one("/home/sprite/rust-desktop/bin/sprite-desktop-gateway");
const daemon = one("/home/sprite/rust-desktop/bin/sprite-desktop-streamd");
const ffmpeg = one("ffmpeg");
const labwc = one("labwc -C");
assert.equal(daemon.parent, gateway.pid, "gateway does not own streamd");
assert.equal(ffmpeg.parent, daemon.pid, "streamd does not own FFmpeg");
assert.notEqual(gateway.pid, daemon.pid);
assert.notEqual(daemon.pid, ffmpeg.pid);

const listeners = await sprite.execFile("ss", ["-H", "-ltn", "sport = :8080"]);
assert.equal(listeners.exitCode, 0, listeners.stderr.toString());
const listenerRows = listeners.stdout
  .toString()
  .trim()
  .split("\n")
  .filter(Boolean);
assert.equal(
  listenerRows.length,
  1,
  "gateway must own one TCP listener on port 8080",
);

const environment = await sprite.execFile("bash", [
  "-lc",
  `tr '\\0' '\\n' </proc/${daemon.pid}/environ`,
]);
assert.equal(environment.exitCode, 0, environment.stderr.toString());
assert.match(environment.stdout.toString(), /^WAYLAND_DISPLAY=wayland-0$/mu);
assert.match(
  environment.stdout.toString(),
  /^XDG_RUNTIME_DIR=\/tmp\/sprite-desktop-rust-session$/mu,
);
const marker = JSON.parse(
  await sprite
    .filesystem("/")
    .readFile("/home/sprite/rust-desktop/trial-owner.json", "utf8"),
) as {
  schema: unknown;
  owner: unknown;
  gatewaySha256: unknown;
  streamdSha256: unknown;
};
assert.equal(marker.schema, 1);
assert.equal(marker.owner, "dev.sprite-desktop.rust-trial");
assert(
  typeof marker.gatewaySha256 === "string" &&
    /^[a-f0-9]{64}$/u.test(marker.gatewaySha256),
);
assert(
  typeof marker.streamdSha256 === "string" &&
    /^[a-f0-9]{64}$/u.test(marker.streamdSha256),
);
const expectedIdentities = new Map([
  [
    "/home/sprite/rust-desktop/bin/sprite-desktop-gateway",
    marker.gatewaySha256,
  ],
  [
    "/home/sprite/rust-desktop/bin/sprite-desktop-streamd",
    marker.streamdSha256,
  ],
  [`/proc/${gateway.pid}/exe`, marker.gatewaySha256],
  [`/proc/${daemon.pid}/exe`, marker.streamdSha256],
]);
const identities = await sprite.execFile("sha256sum", [
  ...expectedIdentities.keys(),
]);
assert.equal(identities.exitCode, 0, identities.stderr.toString());
const actualIdentities = new Map(
  identities.stdout
    .toString()
    .trim()
    .split("\n")
    .map((line) => {
      const match = /^([a-f0-9]{64})  (.+)$/u.exec(line);
      assert(match, "malformed sha256sum output");
      return [match[2]!, match[1]!] as const;
    }),
);
assert.deepEqual(
  actualIdentities,
  expectedIdentities,
  "deployed or running binaries differ from the deployment marker",
);

console.log(
  JSON.stringify(
    {
      sprite: sprite.name,
      suite,
      servicePid: service.state.pid,
      gatewayPid: gateway.pid,
      daemonPid: daemon.pid,
      ffmpegPid: ffmpeg.pid,
      labwcPid: labwc.pid,
      health: "ok",
      listener: listenerRows[0],
      binaries: identities.stdout.toString().trim().split("\n"),
      checkedAt: new Date().toISOString(),
    },
    null,
    2,
  ),
);
