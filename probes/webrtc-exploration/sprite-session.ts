import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { spawn, execFile } from "node:child_process";
import { closeSync, openSync } from "node:fs";
import { readFile, writeFile } from "node:fs/promises";
import { promisify } from "node:util";
import {
  loadTrialSprite,
  sameTrialService,
  trialServiceName,
} from "../rust-desktop/trial.js";

const exec = promisify(execFile);
const statePath = "/tmp/sprite-webrtc-session.json";
const action = process.argv[2];
assert(
  ["start", "check", "stop"].includes(action ?? ""),
  "use start, check or stop",
);
const sprite = await loadTrialSprite("sprite-desktop-rust");
const service = (await sprite.listServices()).find(
  (item) => item.name === trialServiceName,
);
assert(
  service && sameTrialService(service),
  "owned Rust desktop service required",
);
const fs = sprite.filesystem("/");
const inspector = await readFile(
  new URL("remote-processes.py", import.meta.url),
  "utf8",
);
type Identity = { pid: number; ppid: number; startTicks: string; exe: string };
type State = {
  root: string;
  rootPid: number;
  rootStartTicks: string;
  rootExecutable: string;
  binarySha256: string;
  proxyPid: number;
  proxyStartTicks: string;
  originalOutput: unknown;
};
async function remote(file: string, args: string[]) {
  const result = await sprite.execFile(file, args, { timeout: 15_000 });
  assert.equal(result.exitCode, 0, result.stderr.toString());
  return result.stdout.toString();
}
async function descendants(pid: number): Promise<Identity[]> {
  return JSON.parse(
    await remote("python3", ["-c", inspector, String(pid)]),
  ) as Identity[];
}
async function localTicks(pid: number): Promise<string | null> {
  try {
    const stat = await readFile(`/proc/${pid}/stat`, "utf8");
    return stat.slice(stat.lastIndexOf(")") + 2).split(" ")[19] ?? null;
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code === "ENOENT") return null;
    throw error;
  }
}
const wayland = [
  "XDG_RUNTIME_DIR=/tmp/sprite-desktop-rust-session",
  "WAYLAND_DISPLAY=wayland-0",
];
if (action === "start") {
  assert.equal(
    (await exec("ss", ["-H", "-ltn", "sport = :3220"])).stdout.trim(),
    "",
    "local test port is occupied",
  );
  assert.equal(
    (await remote("ss", ["-H", "-ltn", "sport = :3220"])).trim(),
    "",
    "Sprite test port is occupied",
  );
  const hashes = await remote("sha256sum", [
    "/home/sprite/rust-desktop/bin/sprite-desktop-gateway",
    "/home/sprite/rust-desktop/bin/sprite-desktop-streamd",
  ]);
  assert(
    hashes.includes(
      "0c771df4c0fb48d99971efc3133b52a7b3f0dd0474812bd8cd3ba72ddaa6abb4",
    ) &&
      hashes.includes(
        "3341878c62241739569ec6b9b0e435afea1fba12050da5914040f5bca92bab14",
      ),
    "saved WLR pair changed",
  );
  const originalOutput: unknown = JSON.parse(
    await remote("env", [...wayland, "wlr-randr", "--json"]),
  );
  const root = `/home/sprite/webrtc-test-${Date.now()}`;
  assert(!(await fs.exists(root)));
  await fs.mkdir(root, { mode: 0o700 });
  const binary = await readFile(
    new URL("target/release/webrtc-exploration", import.meta.url),
  );
  const binarySha256 = createHash("sha256").update(binary).digest("hex");
  await fs.writeFile(`${root}/webrtc-exploration`, binary, { mode: 0o700 });
  await fs.writeFile(
    `${root}/owner.json`,
    JSON.stringify({
      owner: "sprite-desktop-webrtc-test",
      binarySha256,
      originalOutput,
    }),
    { mode: 0o600 },
  );
  await remote("env", [
    ...wayland,
    "wlr-randr",
    "--output",
    "HEADLESS-1",
    "--custom-mode",
    "1824x848@60",
  ]);
  const rootPid = Number(
    (
      await remote("bash", [
        "-lc",
        'umask 077; cd "$1"; nohup setsid timeout --signal=INT --kill-after=12s 30m ./webrtc-exploration --source desktop </dev/null >server.log 2>&1 & printf "%s\\n" "$!"',
        "webrtc-test",
        root,
      ])
    ).trim(),
  );
  assert(Number.isSafeInteger(rootPid) && rootPid > 1);
  const tree = await descendants(rootPid);
  const rootIdentity = tree.find((item) => item.pid === rootPid);
  assert(
    rootIdentity && rootIdentity.exe.endsWith("/timeout"),
    JSON.stringify(tree),
  );
  const state: State = {
    root,
    rootPid,
    rootStartTicks: rootIdentity.startTicks,
    rootExecutable: rootIdentity.exe,
    binarySha256,
    proxyPid: 0,
    proxyStartTicks: "",
    originalOutput,
  };
  await writeFile(statePath, JSON.stringify(state, null, 2), { mode: 0o600 });
  const log = openSync("/tmp/sprite-webrtc-proxy.log", "a", 0o600);
  const proxy = spawn(
    "sprite",
    ["proxy", "-s", "sprite-desktop-rust", "3220:3220"],
    { detached: true, stdio: ["ignore", log, log] },
  );
  closeSync(log);
  assert(proxy.pid);
  state.proxyPid = proxy.pid;
  state.proxyStartTicks = (await localTicks(proxy.pid))!;
  proxy.unref();
  await writeFile(statePath, JSON.stringify(state, null, 2), { mode: 0o600 });
  for (let attempt = 0; attempt < 20; attempt++) {
    try {
      const response = await fetch("http://127.0.0.1:3220/config", {
        signal: AbortSignal.timeout(1000),
      });
      if (response.ok) break;
    } catch {
      /* tunnel may still be binding */
    }
    await new Promise((resolve) => setTimeout(resolve, 250));
  }
  const config = await (
    await fetch("http://127.0.0.1:3220/config", {
      signal: AbortSignal.timeout(5000),
    })
  ).json();
  assert.equal((config as { source: string }).source, "desktop");
  console.log(
    JSON.stringify({ ...state, url: "http://127.0.0.1:3220", config }, null, 2),
  );
} else {
  const state = JSON.parse(await readFile(statePath, "utf8")) as State;
  assert.match(state.root, /^\/home\/sprite\/webrtc-test-\d+$/u);
  const tree = await descendants(state.rootPid);
  const root = tree.find((item) => item.pid === state.rootPid);
  assert(
    !root ||
      (root.startTicks === state.rootStartTicks &&
        root.exe === state.rootExecutable),
    "owned process identity changed",
  );
  if (action === "check") {
    console.log(JSON.stringify({ root: state.root, tree }, null, 2));
    console.log((await fs.readFile(`${state.root}/server.log`)).toString());
  } else {
    const server = tree.find(
      (item) => item.exe === `${state.root}/webrtc-exploration`,
    );
    if (server) await remote("kill", ["-INT", String(server.pid)]);
    for (let attempt = 0; attempt < 30; attempt++) {
      if ((await descendants(state.rootPid)).length === 0) break;
      await new Promise((resolve) => setTimeout(resolve, 250));
    }
    assert.equal(
      (await descendants(state.rootPid)).length,
      0,
      "owned remote processes remain",
    );
    if (
      state.proxyPid &&
      (await localTicks(state.proxyPid)) === state.proxyStartTicks
    ) {
      process.kill(state.proxyPid, "SIGTERM");
      await new Promise((resolve) => setTimeout(resolve, 500));
    }
    assert.equal(
      (await remote("ss", ["-H", "-ltn", "sport = :3220"])).trim(),
      "",
      "Sprite test port remains open",
    );
    assert.equal(
      (await exec("ss", ["-H", "-ltn", "sport = :3220"])).stdout.trim(),
      "",
      "local test port remains open",
    );
    console.log(JSON.stringify({ stopped: true, root: state.root }));
  }
}
