import assert from "node:assert/strict";
import { parseArgs } from "node:util";
import {
  assertDisposableTarget,
  loadTrialSprite,
  sameTrialService,
  trialServiceName,
} from "./trial.js";

const { values } = parseArgs({
  options: {
    sprite: { type: "string" },
  },
  strict: true,
});
assert(values.sprite, "--sprite is required");
assertDisposableTarget(values.sprite);
const sprite = await loadTrialSprite(values.sprite);
const service = (await sprite.listServices()).find(
  ({ name }) => name === trialServiceName,
);
assert(
  service && sameTrialService(service),
  "owned rust-desktop service is missing",
);
assert.equal(
  service.state?.status,
  "running",
  "rust-desktop service is not running",
);

type ProcessRow = { pid: number; parent: number; command: string };
async function processes(): Promise<ProcessRow[]> {
  const result = await sprite.execFile("ps", ["-eo", "pid=,ppid=,args="]);
  assert.equal(result.exitCode, 0, result.stderr.toString());
  return result.stdout
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
}
function one(rows: ProcessRow[], executable: string): ProcessRow {
  const matches = rows.filter(
    ({ command }) =>
      command === executable || command.startsWith(`${executable} `),
  );
  assert.equal(
    matches.length,
    1,
    `expected one ${executable}, saw ${matches.length}`,
  );
  return matches[0]!;
}
function ssrc(process: ProcessRow): number {
  const match = /(?:^|\s)-ssrc\s+(\d+)(?:\s|$)/u.exec(process.command);
  assert(match, `FFmpeg PID ${process.pid} has no -ssrc argument`);
  return Number(match[1]);
}
async function waitFor<T>(
  read: () => Promise<T | undefined>,
  label: string,
  timeoutMs = 15_000,
): Promise<T> {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    const value = await read();
    if (value !== undefined) return value;
    await new Promise((resolve) => setTimeout(resolve, 100));
  }
  throw new Error(`timed out waiting for ${label}`);
}

const beforeRows = await processes();
const gateway = one(
  beforeRows,
  "/home/sprite/rust-desktop/bin/sprite-desktop-gateway",
);
const daemon = one(
  beforeRows,
  "/home/sprite/rust-desktop/bin/sprite-desktop-streamd",
);
const oldFfmpeg = one(beforeRows, "ffmpeg");
assert.equal(daemon.parent, gateway.pid, "gateway does not own streamd");
assert.equal(oldFfmpeg.parent, daemon.pid, "streamd does not own FFmpeg");
const oldSsrc = ssrc(oldFfmpeg);

const validationRows = await processes();
const validated = validationRows.find(({ pid }) => pid === oldFfmpeg.pid);
assert.deepEqual(
  validated,
  oldFfmpeg,
  "FFmpeg identity changed before termination",
);
const killed = await sprite.execFile("kill", [
  "-KILL",
  "--",
  String(oldFfmpeg.pid),
]);
assert.equal(killed.exitCode, 0, killed.stderr.toString());

const replacement = await waitFor(async () => {
  const rows = await processes();
  if (rows.some(({ pid }) => pid === oldFfmpeg.pid)) return undefined;
  const candidates = rows.filter(
    ({ parent, command }) =>
      parent === daemon.pid && command.startsWith("ffmpeg "),
  );
  if (candidates.length === 0) return undefined;
  assert.equal(
    candidates.length,
    1,
    "streamd owns more than one replacement FFmpeg",
  );
  return candidates[0]!;
}, "replacement FFmpeg");
const newSsrc = ssrc(replacement);
assert.notEqual(newSsrc, oldSsrc, "replacement FFmpeg reused the old SSRC");

console.log(
  JSON.stringify(
    {
      sprite: sprite.name,
      servicePid: service.state?.pid,
      gatewayPid: gateway.pid,
      daemonPid: daemon.pid,
      oldFfmpeg: { pid: oldFfmpeg.pid, ssrc: oldSsrc },
      replacementFfmpeg: { pid: replacement.pid, ssrc: newSsrc },
      checks: ["owned FFmpeg identity", "old process exit", "new SSRC"],
      notChecked: [
        "browser presentation after restart",
        "generation on the wire",
      ],
      checkedAt: new Date().toISOString(),
    },
    null,
    2,
  ),
);
