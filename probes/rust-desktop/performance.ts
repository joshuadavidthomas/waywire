import assert from "node:assert/strict";
import { execFile } from "node:child_process";
import { createHash, randomUUID } from "node:crypto";
import { mkdir, readFile, writeFile } from "node:fs/promises";
import { dirname, resolve } from "node:path";
import { stripTypeScriptTypes } from "node:module";
import { setTimeout as sleep } from "node:timers/promises";
import { parseArgs, promisify } from "node:util";
import type { Sprite } from "@fly/sprites";
import { once } from "node:events";
import { Socket } from "node:net";
import { startTcpSampling } from "./tcp-sampling.js";
import { fileURLToPath } from "node:url";
import { longestFrameGap, hasExpectedQuality } from "./performance-metrics.js";
import { parseStageTrace, summarizeStageTrace } from "./stage-timings.js";
import {
  parseGatewayTimingTrace,
  summarizeGatewayTimingTrace,
} from "./gateway-timings.js";
import {
  parseBrowserTimingTrace,
  summarizeBrowserTimings,
} from "./browser-timings.js";
import {
  createAcceptanceProxy,
  createRustTunnelAcceptanceProxy,
} from "../view-runtime.js";
import { startSpriteTunnel, type SpriteTunnel } from "./sprite-tunnel.js";
import { restartTrial } from "./restart-trial.js";
import {
  assessRecoveryExercise,
  parseRecoveryObservation,
  recoveryExerciseComplete,
  recoveryExerciseTimeoutMs,
} from "./recovery-exercise.js";
import { ProxyVideoTimings } from "./proxy-timings.js";
import {
  startHostSampling,
  type HostSamplingSnapshot,
} from "./host-sampling.js";
import {
  assertDisposableTarget,
  loadTrialSprite,
  sameTrialService,
  trialServiceName,
} from "./trial.js";
import {
  frameCounterConfiguration,
  frameCounterLavfiInput,
  parseFrameCounterTrace,
  summarizeFrameCounter,
} from "./frame-counter.js";

const exec = promisify(execFile);
const proxyPort = 3217;
const viewport = { width: 1867, height: 986 } as const;
const target = { width: 1824, height: 848, refreshHz: 60 } as const;
const recordSeconds = 30;
const thresholds = {
  minimumCanvasUpdateFps: 45,
  maximumP95FrameGapMs: 50,
  maximumFrameGapMs: 250,
  maximumP95LatenessMs: 100,
  maximumP95DecoderQueue: 4,
  minimumClockConfidentSamples: 20,
  minimumClockConfidentFraction: 0.8,
} as const;
const runtimeEnvironment = {
  XDG_RUNTIME_DIR: "/tmp/sprite-desktop-rust-session",
  WAYLAND_DISPLAY: "wayland-0",
} as const;

const { values } = parseArgs({
  options: {
    sprite: { type: "string" },
    output: { type: "string" },
    fidelity: { type: "boolean", default: false },
    stages: { type: "boolean", default: false },
    delivery: { type: "boolean", default: false },
    "frame-counter": { type: "boolean", default: false },
    "exercise-recovery": { type: "boolean", default: false },
    route: { type: "string", default: "public" },
    "restart-trial": { type: "boolean", default: false },
  },
  strict: true,
});
assert(values.sprite, "--sprite is required");
assert(values.output, "--output is required");
assertDisposableTarget(values.sprite);
assert(
  values.route === "public" || values.route === "tunnel",
  "--route must be public or tunnel",
);
const route = values.route;
const spriteName = values.sprite;
const lavfiInput = values["frame-counter"]
  ? frameCounterLavfiInput()
  : "testsrc2=size=1824x848:rate=60";

const outputDirectory = resolve(values.output);
await mkdir(dirname(outputDirectory), { recursive: true });
await mkdir(outputDirectory);
const observerPath = resolve(outputDirectory, "browser-observer.js");
await writeFile(
  observerPath,
  stripTypeScriptTypes(
    await readFile(new URL("browser-observer.ts", import.meta.url), "utf8"),
  ),
  { flag: "wx" },
);

const nonce = randomUUID();
const session = `sprite-rust-performance-${process.pid}-${nonce.slice(0, 8)}`;
const title = `sprite-rust-performance-${nonce}`;
const remoteLog = `/tmp/${title}.log`;
const remoteCaptures = new Set<string>();
let sprite: Sprite | undefined;
let proxy: ReturnType<typeof createAcceptanceProxy> | undefined;
const proxyObservers: ProxyVideoTimings[] = [];
const upstreamPorts: number[] = [];
let tcpSampling: ReturnType<typeof startTcpSampling> | undefined;
let tunnelTcpSampling: ReturnType<typeof startTcpSampling> | undefined;
let tunnel: SpriteTunnel | undefined;
let proxyObserverOverflow = 0;
let deliveryActive = false;
let hostSampling: Awaited<ReturnType<typeof startHostSampling>> | undefined;
let deliverySnapshot:
  | {
      schema: "sprite-desktop-delivery-v1";
      streams: ReturnType<ProxyVideoTimings["finish"]>[];
      observerOverflow: number;
      host: HostSamplingSnapshot;
      tcp?: Awaited<ReturnType<ReturnType<typeof startTcpSampling>["finish"]>>;
      route: "public" | "tunnel";
      tunnelTcp?: Awaited<
        ReturnType<ReturnType<typeof startTcpSampling>["finish"]>
      >;
    }
  | undefined;
async function finishDelivery() {
  if (!deliveryActive || !hostSampling) return;
  deliveryActive = false;
  const streams = proxyObservers.map((observer) => observer.finish());
  deliverySnapshot = {
    schema: "sprite-desktop-delivery-v1",
    streams,
    observerOverflow: proxyObserverOverflow,
    host: await hostSampling.finish(),
    ...(tcpSampling ? { tcp: await tcpSampling.finish() } : {}),
    route,
    ...(tunnelTcpSampling
      ? { tunnelTcp: await tunnelTcpSampling.finish() }
      : {}),
  };
  await writeFile(
    resolve(outputDirectory, "delivery.json"),
    `${JSON.stringify(deliverySnapshot, null, 2)}\n`,
    { flag: "wx" },
  );
}
let browserOpened = false;
let ffplayPid: number | undefined;
let primaryError: unknown;
let gatePassed = false;
let recoveryPassed = true;
let stageTopology: Topology | undefined;
let stageTimingActive = false;
let stageStartedNanos: bigint | undefined;
const cleanup: Record<string, unknown> = {
  browserSession: session,
  browserClosed: false,
  proxyClosed: false,
  ffplay: "not launched",
};
const report: Record<string, unknown> = {
  schema: 1,
  status: "FAILED",
  sprite: values.sprite,
  runId: nonce,
  startedAt: new Date().toISOString(),
  route: values.route,
  restartRequested: values["restart-trial"],
  recoveryExerciseRequested: values["exercise-recovery"],
  frameCounterRequested: values["frame-counter"],
  workload: {
    name: "continuous motion",
    generator: values["frame-counter"]
      ? "ffplay renders FFmpeg lavfi testsrc2 at native 1824x848 and 60 Hz with a probe-only 16-bit source-frame strip in the top-left corner. The browser only observes and decodes that native application."
      : "ffplay renders FFmpeg lavfi testsrc2 at native 1824x848 and 60 Hz in a fullscreen Wayland window. The browser only observes and decodes that native application.",
    command: `ffplay -f lavfi -i ${lavfiInput} -fs -autoexit (fixed owned process; no caller-supplied command)`,
    nativeTarget: target,
    browserViewport: viewport,
    recordSeconds,
  },
  thresholds,
};

interface BrowserReply<T> {
  readonly success: boolean;
  readonly data: T;
  readonly error?: unknown;
}

interface CanvasObservation {
  readonly connected: boolean;
  readonly width: number;
  readonly height: number;
  readonly cssWidth: number;
  readonly cssHeight: number;
  readonly colors: number;
  readonly sampleHash: number;
  readonly metrics: string;
  readonly recorderReady: boolean;
}

interface Dimensions {
  readonly width: number;
  readonly height: number;
  readonly cssWidth: number;
  readonly cssHeight: number;
  readonly devicePixelRatio: number;
}

interface RecorderStats {
  readonly atMs: number;
  readonly width: number;
  readonly height: number;
  readonly renderedFps: number;
  readonly renderedMediaTimestampMicros: number;
  readonly drawCompletedAtMs: number;
  readonly generation: number;
  readonly bitrateKbps: number;
  readonly scalePercent: number;
  readonly rttMs: number;
  readonly clockConfident: boolean;
  readonly clockUncertaintyMs: number | null;
  readonly latenessMs: number;
  readonly decoderQueue: number;
  readonly receivedFrames: number;
  readonly decodedFrames: number;
  readonly presentedFrames: number;
  readonly droppedFrames: number;
  readonly overdueDroppedFrames: number;
  readonly decodedOverflowDroppedFrames: number;
  readonly decoderResetDroppedFrames: number;
}

interface Distribution {
  readonly count: number;
  readonly p50: number | null;
  readonly p95: number | null;
  readonly p99: number | null;
  readonly max: number | null;
}

interface RecorderSummary {
  readonly durationSeconds: number;
  readonly canvasUpdateFrames: number;
  readonly canvasUpdateFramesPerSecond: number | null;
  readonly canvasUpdateFrameSpacingMs: Distribution;
  readonly canvasUpdateFrameGapsOver100ms: number;
  readonly canvasSubmits: number;
  readonly canvasSubmitsPerSecond: number | null;
  readonly receivedPayloadMbps: number;
  readonly websocketReceivedPayloadBytes: number;
  readonly warnings: readonly string[];
}

interface Recording {
  readonly schema: number;
  readonly transport: string;
  readonly workload: string;
  readonly startedAt: string;
  readonly endedAt: string;
  readonly durationMs: number;
  readonly dimensions: Dimensions;
  readonly finalDimensions: Dimensions;
  readonly canvasSubmitsMs: readonly number[];
  readonly canvasUpdateFramesMs: readonly number[];
  readonly sdkStats: readonly RecorderStats[];
  readonly receivedBytes: number;
  readonly warnings: readonly string[];
  readonly summary: RecorderSummary;
}

interface ProcessRow {
  readonly pid: number;
  readonly parent: number;
  readonly command: string;
}

interface Topology {
  readonly servicePid: number;
  readonly gatewayPid: number;
  readonly daemonPid: number;
  readonly ffmpegPid: number;
  readonly labwcPid: number;
  readonly ffmpegCommand: string;
}

async function browser<T>(...args: string[]): Promise<T> {
  const { stdout } = await exec(
    "agent-browser",
    ["--session", session, "--init-script", observerPath, "--json", ...args],
    { timeout: 75_000, maxBuffer: 20 * 1024 * 1024 },
  );
  const reply = JSON.parse(stdout) as BrowserReply<T>;
  assert(reply.success, JSON.stringify(reply.error));
  return reply.data;
}

async function evaluate<T>(expression: string): Promise<T> {
  const data = await browser<{ result: T }>("eval", expression);
  return data.result;
}

async function runRecoveryExercise() {
  parseRecoveryObservation(
    await evaluate<unknown>("window.__rustVideoObserver.startRecovery()"),
  );
  const deadline = Date.now() + recoveryExerciseTimeoutMs;
  while (Date.now() < deadline) {
    const observation = parseRecoveryObservation(
      await evaluate<unknown>("window.__rustVideoObserver.recoveryStatus()"),
    );
    if (recoveryExerciseComplete(observation)) break;
    await sleep(50);
  }
  return assessRecoveryExercise(
    await evaluate<unknown>("window.__rustVideoObserver.finishRecovery()"),
  );
}

async function remote(
  file: string,
  args: readonly string[],
  options: {
    readonly env?: Record<string, string>;
    readonly timeout?: number;
  } = {},
) {
  assert(sprite, "Sprite is not loaded");
  const result = await sprite.execFile(file, [...args], options);
  assert.equal(result.exitCode, 0, result.stderr.toString());
  return result;
}

function parseProcesses(text: string): ProcessRow[] {
  return text
    .trim()
    .split("\n")
    .filter(Boolean)
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

function oneProcess(
  rows: readonly ProcessRow[],
  executable: string,
): ProcessRow {
  const matches = rows.filter(
    ({ command }) =>
      command === executable || command.startsWith(`${executable} `),
  );
  assert.equal(
    matches.length,
    1,
    `expected one ${executable} process, saw ${matches.length}`,
  );
  return matches[0]!;
}

async function readTopology(): Promise<Topology> {
  assert(sprite);
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
  assert(service.state.pid && service.state.pid > 1, "service has no live PID");
  const processResult = await remote("ps", ["-eo", "pid=,ppid=,args="]);
  const rows = parseProcesses(processResult.stdout.toString());
  const gateway = oneProcess(
    rows,
    "/home/sprite/rust-desktop/bin/sprite-desktop-gateway",
  );
  const daemon = oneProcess(
    rows,
    "/home/sprite/rust-desktop/bin/sprite-desktop-streamd",
  );
  const ffmpeg = oneProcess(rows, "ffmpeg");
  const labwc = oneProcess(rows, "labwc -C");
  assert.equal(daemon.parent, gateway.pid, "gateway does not own streamd");
  assert.equal(ffmpeg.parent, daemon.pid, "streamd does not own FFmpeg");
  return {
    servicePid: service.state.pid,
    gatewayPid: gateway.pid,
    daemonPid: daemon.pid,
    ffmpegPid: ffmpeg.pid,
    labwcPid: labwc.pid,
    ffmpegCommand: ffmpeg.command,
  };
}

async function readStageTimingPaths(topology: Topology) {
  const current = await readTopology();
  assert.equal(
    current.daemonPid,
    topology.daemonPid,
    "streamd changed before stage tracing",
  );
  assert.equal(
    current.gatewayPid,
    topology.gatewayPid,
    "gateway changed before stage tracing",
  );
  const readEnvironmentPath = async (pid: number, key: string) => {
    const result = await remote("python3", [
      "-c",
      "import sys; key=(sys.argv[2]+'=').encode(); values=[item[len(key):] for item in open('/proc/'+sys.argv[1]+'/environ','rb').read().split(b'\\0') if item.startswith(key)]; assert len(values)==1; sys.stdout.buffer.write(values[0])",
      String(pid),
      key,
    ]);
    return result.stdout.toString();
  };
  const native = await readEnvironmentPath(
    current.daemonPid,
    "SPRITE_DESKTOP_STAGE_TIMINGS",
  );
  const gateway = await readEnvironmentPath(
    current.gatewayPid,
    "SPRITE_DESKTOP_GATEWAY_STAGE_TIMINGS",
  );
  assert.match(
    native,
    /^\/tmp\/sprite-desktop-rust-session\/stage-timings\.[A-Za-z0-9]{8}\/stages\.json$/u,
  );
  assert.equal(
    gateway,
    `${dirname(native)}/gateway.json`,
    "trace files do not share the owned directory",
  );
  const ownership = await remote("stat", ["-c", "%U %a %F", dirname(native)]);
  assert.equal(ownership.stdout.toString(), "sprite 700 directory\n");
  for (const path of [native, gateway]) {
    const target = await remote("stat", ["-c", "%U %a %F", path]);
    assert.match(
      target.stdout.toString(),
      /^sprite 600 regular(?: empty)? file\n$/u,
    );
  }
  return { native, gateway };
}

async function signalStageTiming(topology: Topology, signal: "USR1" | "USR2") {
  const current = await readTopology();
  assert.equal(
    current.daemonPid,
    topology.daemonPid,
    `streamd changed before SIG${signal}`,
  );
  assert.equal(
    current.gatewayPid,
    topology.gatewayPid,
    `gateway changed before SIG${signal}`,
  );
  const result = await remote("python3", [
    "-c",
    "import os,signal,sys,time; before=time.monotonic_ns(); sig=getattr(signal,'SIG'+sys.argv[3]); os.kill(int(sys.argv[1]),sig); os.kill(int(sys.argv[2]),sig); print(before)",
    String(current.daemonPid),
    String(current.gatewayPid),
    signal,
  ]);
  return BigInt(result.stdout.toString().trim());
}

async function readStageTimingFile(
  path: string,
  startedNanos: bigint,
): Promise<string> {
  assert(sprite);
  const deadline = Date.now() + 2_500;
  let lastError: unknown;
  while (Date.now() < deadline) {
    try {
      const sizeResult = await remote("stat", ["-c", "%s", path]);
      const size = Number(sizeResult.stdout.toString().trim());
      assert(
        Number.isSafeInteger(size) && size > 0 && size <= 8 * 1024 * 1024,
        "stage trace has an invalid size",
      );
      const bytes: string = await sprite.filesystem("/").readFile(path);
      assert.equal(bytes.length, size, "stage trace changed while being read");
      const trace = parseStageTrace(JSON.parse(bytes));
      assert(
        trace.records.some(
          (record) => BigInt(record.timestamp_nanos) >= startedNanos,
        ),
        "waiting for this run's stage snapshot, not a previous dump",
      );
      return bytes;
    } catch (error) {
      lastError = error;
      await sleep(50);
    }
  }
  throw lastError ?? new Error("stage trace was not written");
}

async function readGatewayTimingFile(
  path: string,
  startedNanos: bigint,
): Promise<string> {
  assert(sprite);
  const deadline = Date.now() + 2_500;
  let lastError: unknown;
  while (Date.now() < deadline) {
    try {
      const sizeResult = await remote("stat", ["-c", "%s", path]);
      const size = Number(sizeResult.stdout.toString().trim());
      assert(
        Number.isSafeInteger(size) && size > 0 && size <= 16 * 1024 * 1024,
        "gateway trace has an invalid size",
      );
      const bytes: string = await sprite.filesystem("/").readFile(path);
      assert.equal(
        bytes.length,
        size,
        "gateway trace changed while being read",
      );
      const trace = parseGatewayTimingTrace(JSON.parse(bytes));
      assert(
        trace.records.some(
          (record) => BigInt(record.timestamp_nanos) >= startedNanos,
        ),
        "waiting for this run's gateway snapshot",
      );
      return bytes;
    } catch (error) {
      lastError = error;
      await sleep(50);
    }
  }
  throw lastError ?? new Error("gateway trace was not written");
}

async function checkPrerequisites(): Promise<string> {
  const commandCheck = await remote("bash", [
    "-lc",
    'for command in ffplay ffmpeg grim python3 wlr-randr; do command -v "$command" >/dev/null || { echo missing:$command; exit 1; }; done',
  ]);
  assert.equal(commandCheck.stdout.toString(), "");
  const health = await remote("curl", [
    "-fsS",
    "--max-time",
    "5",
    "http://127.0.0.1:8080/healthz",
  ]);
  assert.equal(health.stdout.toString(), "ok\n");
  const listeners = await remote("ss", ["-H", "-ltn", "sport = :8080"]);
  assert.equal(
    listeners.stdout.toString().trim().split("\n").filter(Boolean).length,
    1,
    "gateway must keep one listener on port 8080",
  );
  const output = await remote("wlr-randr", [], { env: runtimeEnvironment });
  const text = output.stdout.toString();
  return text;
}

async function launchFfplay(): Promise<number> {
  const launch = await remote(
    "bash",
    [
      "-lc",
      'umask 077; nohup setsid env XDG_RUNTIME_DIR=/tmp/sprite-desktop-rust-session WAYLAND_DISPLAY=wayland-0 SDL_VIDEODRIVER=wayland ffplay -hide_banner -loglevel error -f lavfi -i "$3" -fs -autoexit -t 75 -window_title "$1" </dev/null >"$2" 2>&1 & printf "%s\\n" "$!"',
      "sprite-performance",
      title,
      remoteLog,
      lavfiInput,
    ],
    { timeout: 10_000 },
  );
  const pid = Number(launch.stdout.toString().trim());
  assert(
    Number.isInteger(pid) && pid > 1,
    "ffplay did not return an owned PID",
  );
  ffplayPid = pid;
  await sleep(750);
  const identity = await remote("python3", [
    "-c",
    "import json,os,sys; p=int(sys.argv[1]); raw=open(f'/proc/{p}/cmdline','rb').read().split(b'\\0'); print(json.dumps([x.decode(errors='replace') for x in raw if x]))",
    String(pid),
  ]);
  const command = JSON.parse(identity.stdout.toString()) as string[];
  assert(
    command.some((part) => part.endsWith("ffplay")),
    `PID ${pid} is not ffplay`,
  );
  assert(command.includes(title), `PID ${pid} lacks the unique window title`);
  return pid;
}

async function waitForStableCanvas(): Promise<CanvasObservation[]> {
  const observations: CanvasObservation[] = [];
  const deadline = Date.now() + 15_000;
  while (Date.now() < deadline) {
    const observation = await evaluate<CanvasObservation>(`(() => {
      const canvas = document.querySelector("#display");
      const panel = document.querySelector('section[aria-label="Comparison recording"]');
      if (!(canvas instanceof HTMLCanvasElement)) return { connected: false, width: 0, height: 0, cssWidth: 0, cssHeight: 0, colors: 0, sampleHash: 0, metrics: "", recorderReady: false };
      const rect = canvas.getBoundingClientRect();
      const context = canvas.getContext("2d");
      let colors = 0;
      let sampleHash = 2166136261;
      if (context && canvas.width > 0 && canvas.height > 0) {
        const seen = new Set();
        for (let y = 0; y < 12; y += 1) for (let x = 0; x < 20; x += 1) {
          const pixel = context.getImageData(Math.floor((x + 0.5) * canvas.width / 20), Math.floor((y + 0.5) * canvas.height / 12), 1, 1).data;
          const color = pixel[0] + "," + pixel[1] + "," + pixel[2] + "," + pixel[3];
          seen.add(color);
          for (const value of pixel) sampleHash = Math.imul(sampleHash ^ value, 16777619) >>> 0;
        }
        colors = seen.size;
      }
      return {
        connected: document.querySelector("#status.connected") !== null && document.querySelector("#empty.hidden") !== null,
        width: canvas.width,
        height: canvas.height,
        cssWidth: rect.width,
        cssHeight: rect.height,
        colors,
        sampleHash,
        metrics: document.querySelector("#metrics")?.textContent ?? "",
        recorderReady: panel !== null,
      };
    })()`);
    observations.push(observation);
    const recent = observations.slice(-5);
    if (
      recent.length === 5 &&
      recent.every(
        (sample) =>
          sample.connected &&
          sample.recorderReady &&
          sample.width === target.width &&
          sample.height === target.height &&
          sample.colors >= 24,
      ) &&
      new Set(recent.map(({ sampleHash }) => sampleHash)).size >= 3
    )
      return recent;
    await sleep(1000);
  }
  throw new Error(
    `viewer did not reach a changing ${target.width}x${target.height} canvas: ${JSON.stringify(observations.slice(-5))}`,
  );
}

async function captureNative(
  name: string,
): Promise<{ path: string; bytes: number; sha256: string; signal: string }> {
  assert(sprite);
  const remotePath = `/tmp/${title}-${name}.png`;
  remoteCaptures.add(remotePath);
  await remote("grim", [remotePath], { env: runtimeEnvironment });
  const signal = await remote("ffmpeg", [
    "-hide_banner",
    "-v",
    "error",
    "-i",
    remotePath,
    "-vf",
    "signalstats,metadata=print:file=-",
    "-frames:v",
    "1",
    "-f",
    "null",
    "-",
  ]);
  const signalText = `${signal.stdout}${signal.stderr}`;
  const yMinimum = /lavfi\.signalstats\.YMIN=(\d+)/u.exec(signalText);
  const yMaximum = /lavfi\.signalstats\.YMAX=(\d+)/u.exec(signalText);
  assert(
    yMinimum && yMaximum,
    "FFmpeg did not report native screenshot signal statistics",
  );
  assert.notEqual(
    yMinimum[1],
    yMaximum[1],
    "native screenshot is flat luminance",
  );
  const image = await sprite.filesystem("/").readFile(remotePath);
  const localPath = resolve(outputDirectory, `${name}-native.png`);
  await writeFile(localPath, image, { flag: "wx" });
  return {
    path: localPath,
    bytes: image.length,
    sha256: createHash("sha256").update(image).digest("hex"),
    signal: signalText.trim(),
  };
}

async function captureBrowser(
  name: string,
): Promise<{ path: string; bytes: number; sha256: string }> {
  const path = resolve(outputDirectory, `${name}-browser.png`);
  await browser("screenshot", path);
  const image = await readFile(path);
  assert(image.length > 10_000, "browser screenshot is unexpectedly small");
  return {
    path,
    bytes: image.length,
    sha256: createHash("sha256").update(image).digest("hex"),
  };
}

const cpuSampler = String.raw`
import json, os, sys, time
seconds = int(sys.argv[1])
names = json.loads(sys.argv[2])
hz = os.sysconf(os.sysconf_names['SC_CLK_TCK'])

def total():
    values = [int(x) for x in open('/proc/stat').readline().split()[1:9]]
    return sum(values), values[3] + values[4]

def process(pid):
    try:
        fields = open(f'/proc/{pid}/stat').read().split()
        return int(fields[13]) + int(fields[14]), int(fields[23]) * os.sysconf('SC_PAGE_SIZE')
    except (FileNotFoundError, ProcessLookupError):
        return None

previous_at = time.monotonic()
previous_total, previous_idle = total()
previous_processes = {name: process(pid) for name, pid in names.items()}
samples = []
for _ in range(seconds):
    time.sleep(1)
    now = time.monotonic()
    current_total, current_idle = total()
    elapsed = now - previous_at
    total_delta = current_total - previous_total
    busy_delta = total_delta - (current_idle - previous_idle)
    processes = {}
    current_processes = {}
    for name, pid in names.items():
        current = process(pid)
        current_processes[name] = current
        previous = previous_processes[name]
        processes[name] = None if current is None or previous is None else {
            'cpuPercentOneCore': (current[0] - previous[0]) * 100 / hz / elapsed,
            'rssBytes': current[1],
        }
    samples.append({
        'atEpochMs': time.time() * 1000,
        'intervalSeconds': elapsed,
        'wholeSpriteCpuPercentAllCpus': None if total_delta <= 0 else busy_delta * 100 / total_delta,
        'processes': processes,
    })
    previous_at = now
    previous_total, previous_idle = current_total, current_idle
    previous_processes = current_processes
print(json.dumps({
    'schema': 1,
    'scope': 'Whole-Sprite /proc/stat plus read-only per-process /proc samples. Per-process CPU uses one logical core as 100%. ffplay workload cost is separate from the gateway, streamd, encoder FFmpeg, and labwc.',
    'samples': samples,
}))
`;

async function sampleCpu(topology: Topology, workloadPid: number) {
  const names = {
    gateway: topology.gatewayPid,
    streamd: topology.daemonPid,
    encoderFfmpeg: topology.ffmpegPid,
    labwc: topology.labwcPid,
    workloadFfplay: workloadPid,
  };
  const result = await remote(
    "python3",
    ["-c", cpuSampler, String(recordSeconds), JSON.stringify(names)],
    { timeout: (recordSeconds + 10) * 1000 },
  );
  return JSON.parse(result.stdout.toString()) as {
    readonly schema: number;
    readonly scope: string;
    readonly samples: readonly {
      readonly wholeSpriteCpuPercentAllCpus: number | null;
      readonly processes: Readonly<
        Record<
          string,
          {
            readonly cpuPercentOneCore: number;
            readonly rssBytes: number;
          } | null
        >
      >;
    }[];
  };
}

function distribution(values: readonly number[]): Distribution {
  const sorted = values
    .filter(Number.isFinite)
    .toSorted((left, right) => left - right);
  const at = (fraction: number) =>
    sorted.length ? sorted[Math.ceil(fraction * sorted.length) - 1]! : null;
  return {
    count: sorted.length,
    p50: at(0.5),
    p95: at(0.95),
    p99: at(0.99),
    max: sorted.at(-1) ?? null,
  };
}

function cpuSummary(cpu: Awaited<ReturnType<typeof sampleCpu>>) {
  const processNames = [
    "gateway",
    "streamd",
    "encoderFfmpeg",
    "labwc",
    "workloadFfplay",
  ];
  return {
    scope: cpu.scope,
    sampleCount: cpu.samples.length,
    wholeSpriteCpuPercentAllCpus: distribution(
      cpu.samples.flatMap(({ wholeSpriteCpuPercentAllCpus }) =>
        wholeSpriteCpuPercentAllCpus === null
          ? []
          : [wholeSpriteCpuPercentAllCpus],
      ),
    ),
    processes: Object.fromEntries(
      processNames.map((name) => [
        name,
        distribution(
          cpu.samples.flatMap(({ processes }) => {
            const sample = processes[name];
            return sample ? [sample.cpuPercentOneCore] : [];
          }),
        ),
      ]),
    ),
  };
}

function counterDelta(
  stats: readonly RecorderStats[],
  field: keyof Pick<
    RecorderStats,
    | "receivedFrames"
    | "decodedFrames"
    | "presentedFrames"
    | "droppedFrames"
    | "overdueDroppedFrames"
    | "decodedOverflowDroppedFrames"
    | "decoderResetDroppedFrames"
  >,
) {
  const values = stats.map((sample) => sample[field]);
  assert(
    values.every((value) => Number.isSafeInteger(value) && value >= 0),
    `${field} is not a nonnegative counter`,
  );
  assert(
    values.every((value, index) => index === 0 || value >= values[index - 1]!),
    `${field} decreased during the recording`,
  );
  return {
    first: values.at(0) ?? null,
    last: values.at(-1) ?? null,
    intervalDelta: values.length ? values.at(-1)! - values[0]! : null,
  };
}

function recordingEvaluation(recording: Recording) {
  assert.equal(recording.schema, 1, "unexpected recorder schema");
  assert.equal(
    recording.transport,
    "rust",
    "recorder used the wrong transport",
  );
  assert.equal(
    recording.workload,
    "continuous motion",
    "recorder used the wrong workload label",
  );
  const stats = recording.sdkStats;
  const confident = stats.filter(({ clockConfident }) => clockConfident);
  const generations = [...new Set(stats.map(({ generation }) => generation))];
  const widths = stats.map(({ width }) => width);
  const heights = stats.map(({ height }) => height);
  const evaluation = {
    actualRecordingDimensions: {
      initial: recording.dimensions,
      final: recording.finalDimensions,
      sdkMinimum: {
        width: Math.min(...widths),
        height: Math.min(...heights),
      },
      sdkMaximum: {
        width: Math.max(...widths),
        height: Math.max(...heights),
      },
    },
    canvasUpdateFps: recording.summary.canvasUpdateFramesPerSecond,
    canvasFrameGapMs: recording.summary.canvasUpdateFrameSpacingMs,
    longestFreezeMs: longestFrameGap(
      recording.durationMs,
      recording.canvasUpdateFramesMs,
    ),
    sdkRenderedFps: distribution(stats.map(({ renderedFps }) => renderedFps)),
    clock: {
      confidentSamples: confident.length,
      totalSamples: stats.length,
      confidentFraction: stats.length ? confident.length / stats.length : 0,
      uncertaintyMs: distribution(
        confident.flatMap(({ clockUncertaintyMs }) =>
          clockUncertaintyMs === null ? [] : [clockUncertaintyMs],
        ),
      ),
    },
    latenessMs: distribution(confident.map(({ latenessMs }) => latenessMs)),
    decoderQueue: distribution(stats.map(({ decoderQueue }) => decoderQueue)),
    monotonicFrameCounters: {
      received: counterDelta(stats, "receivedFrames"),
      decoded: counterDelta(stats, "decodedFrames"),
      presented: counterDelta(stats, "presentedFrames"),
      dropped: counterDelta(stats, "droppedFrames"),
      dropCauses: {
        overdue: counterDelta(stats, "overdueDroppedFrames"),
        decodedOverflow: counterDelta(stats, "decodedOverflowDroppedFrames"),
        decoderReset: counterDelta(stats, "decoderResetDroppedFrames"),
      },
      scope:
        "Deltas span the first and last recorder samples, not the exact 30-second edges. They are strict session-monotonic counters; frames before the first sample or after the last sample are excluded.",
    },
    generations,
    payload: {
      bytes: recording.summary.websocketReceivedPayloadBytes,
      megabitsPerSecond: recording.summary.receivedPayloadMbps,
    },
    recorderWarnings: recording.summary.warnings,
  };
  const checks: {
    name: string;
    passed: boolean;
    actual: unknown;
    requirement: string;
  }[] = [
    {
      name: "30-second recording completed",
      passed: recording.durationMs >= 29_500,
      actual: recording.durationMs,
      requirement: ">= 29500 ms",
    },
    {
      name: "continuous-motion canvas update rate",
      passed:
        recording.summary.canvasUpdateFramesPerSecond !== null &&
        recording.summary.canvasUpdateFramesPerSecond >=
          thresholds.minimumCanvasUpdateFps,
      actual: recording.summary.canvasUpdateFramesPerSecond,
      requirement: `>= ${thresholds.minimumCanvasUpdateFps} FPS`,
    },
    {
      name: "canvas update frame-gap p95",
      passed:
        recording.summary.canvasUpdateFrameSpacingMs.p95 !== null &&
        recording.summary.canvasUpdateFrameSpacingMs.p95 <=
          thresholds.maximumP95FrameGapMs,
      actual: recording.summary.canvasUpdateFrameSpacingMs.p95,
      requirement: `<= ${thresholds.maximumP95FrameGapMs} ms`,
    },
    {
      name: "longest canvas freeze",
      passed: evaluation.longestFreezeMs <= thresholds.maximumFrameGapMs,
      actual: evaluation.longestFreezeMs,
      requirement: `<= ${thresholds.maximumFrameGapMs} ms`,
    },
    {
      name: "clock confidence",
      passed:
        confident.length >= thresholds.minimumClockConfidentSamples &&
        confident.length / Math.max(1, stats.length) >=
          thresholds.minimumClockConfidentFraction,
      actual: `${confident.length}/${stats.length}`,
      requirement: `>= ${thresholds.minimumClockConfidentSamples} and >= ${thresholds.minimumClockConfidentFraction}`,
    },
    {
      name: "clock-confident lateness p95",
      passed:
        evaluation.latenessMs.p95 !== null &&
        evaluation.latenessMs.p95 <= thresholds.maximumP95LatenessMs,
      actual: evaluation.latenessMs.p95,
      requirement: `<= ${thresholds.maximumP95LatenessMs} ms`,
    },
    {
      name: "decoder queue p95",
      passed:
        evaluation.decoderQueue.p95 !== null &&
        evaluation.decoderQueue.p95 <= thresholds.maximumP95DecoderQueue,
      actual: evaluation.decoderQueue.p95,
      requirement: `<= ${thresholds.maximumP95DecoderQueue}`,
    },
    {
      name: "stable current generation",
      passed: generations.length === 1 && (generations[0] ?? 0) > 0,
      actual: generations,
      requirement: "one positive generation",
    },
    {
      name: "native-size canvas throughout",
      passed:
        stats.length > 0 &&
        widths.every((width) => width === target.width) &&
        heights.every((height) => height === target.height) &&
        recording.dimensions.width === target.width &&
        recording.dimensions.height === target.height &&
        recording.finalDimensions.width === target.width &&
        recording.finalDimensions.height === target.height,
      actual: evaluation.actualRecordingDimensions,
      requirement: `${target.width}x${target.height} throughout`,
    },
    {
      name: "live WebSocket payload",
      passed: recording.summary.websocketReceivedPayloadBytes > 0,
      actual: evaluation.payload,
      requirement: "> 0 bytes",
    },
  ];
  return { evaluation, checks };
}

async function stopOwnedFfplay(pid: number): Promise<Record<string, unknown>> {
  assert(sprite);
  const cleanupProgram = String.raw`
import json, os, signal, sys, time
pid = int(sys.argv[1])
title = sys.argv[2]
try:
    pidfd = os.pidfd_open(pid)
except ProcessLookupError:
    print(json.dumps({'status': 'already exited', 'pid': pid}))
    raise SystemExit(0)
try:
    raw = open(f'/proc/{pid}/cmdline', 'rb').read().split(b'\0')
    command = [part.decode(errors='replace') for part in raw if part]
    if not any(part.endswith('ffplay') for part in command) or title not in command:
        print(json.dumps({'status': 'identity mismatch; not signalled', 'pid': pid, 'command': command}))
        raise SystemExit(3)
    signal.pidfd_send_signal(pidfd, signal.SIGTERM)
    deadline = time.monotonic() + 3
    while time.monotonic() < deadline:
        try:
            os.kill(pid, 0)
        except ProcessLookupError:
            print(json.dumps({'status': 'terminated', 'pid': pid, 'signal': 'SIGTERM'}))
            raise SystemExit(0)
        time.sleep(.05)
    signal.pidfd_send_signal(pidfd, signal.SIGKILL)
    print(json.dumps({'status': 'terminated', 'pid': pid, 'signal': 'SIGKILL after validated SIGTERM timeout'}))
finally:
    os.close(pidfd)
`;
  const result = await sprite.execFile("python3", [
    "-c",
    cleanupProgram,
    String(pid),
    title,
  ]);
  assert.equal(
    result.exitCode,
    0,
    result.stdout.toString() || result.stderr.toString(),
  );
  return JSON.parse(result.stdout.toString()) as Record<string, unknown>;
}

// The native probe verifies installed/live binaries against the saved marker.
// Its gateway hash also identifies the embedded viewer.
async function inspectDeployment() {
  const identity = await exec(
    "pnpm",
    [
      "exec",
      "tsx",
      fileURLToPath(new URL("native-check.ts", import.meta.url)),
      "--sprite",
      spriteName,
      "--suite",
      "video",
    ],
    { timeout: 30_000, maxBuffer: 1024 * 1024 },
  );
  return JSON.parse(identity.stdout) as {
    binaries: string[];
    gatewayPid: number;
    daemonPid: number;
    labwcPid: number;
  };
}

try {
  sprite = await loadTrialSprite(values.sprite);
  const identity = await inspectDeployment();
  if (values["restart-trial"]) {
    report.beforeRestart = identity;
    report.restart = await restartTrial(sprite);
    const restarted = await inspectDeployment();
    assert.deepEqual(
      restarted.binaries.map((line) => line.split(/\s+/u)[0]),
      identity.binaries.map((line) => line.split(/\s+/u)[0]),
      "installed/live binary hashes changed across trial restart",
    );
    for (const field of ["gatewayPid", "daemonPid", "labwcPid"] as const)
      assert.notEqual(
        restarted[field],
        identity[field],
        `${field} did not change after service recreation`,
      );
    report.deployment = restarted;
  } else report.deployment = identity;
  report.topologyAtStart = await readTopology();
  report.nativeOutputBefore = await checkPrerequisites();

  if (values.route === "tunnel") {
    tunnel = await startSpriteTunnel(values.sprite);
    report.tunnel = { pid: tunnel.pid, port: tunnel.port, remotePort: 8080 };
  }
  const observe: Parameters<typeof createRustTunnelAcceptanceProxy>[2] =
    values.delivery
      ? (upstream) => {
          if (proxyObservers.length >= 4) {
            proxyObserverOverflow += 1;
            return () => {};
          }
          if (upstream instanceof Socket && upstream.localPort)
            upstreamPorts.push(upstream.localPort);
          const observer = new ProxyVideoTimings();
          proxyObservers.push(observer);
          if (deliveryActive) observer.start();
          return (chunk) => observer.receive(chunk);
        }
      : undefined;
  proxy = tunnel
    ? createRustTunnelAcceptanceProxy(sprite.url!, tunnel.port, observe)
    : createAcceptanceProxy(
        sprite.url!,
        process.env.SPRITES_TOKEN!,
        "rust",
        undefined,
        observe,
      );
  proxy.server.listen(proxyPort, "127.0.0.1");
  await once(proxy.server, "listening");

  browserOpened = true;
  await browser(
    "set",
    "viewport",
    String(viewport.width),
    String(viewport.height),
  );
  await browser("open", `http://127.0.0.1:${proxyPort}/?record`);
  // agent-browser 0.33.2's domain guard removes WebSocket's static constants.
  // That breaks the SDK's ready-state checks, so use an unmodified browser
  // against the fixed token-isolating proxy and reject altered WebSocket APIs.
  assert.deepEqual(
    await evaluate<number[]>(
      "[WebSocket.CONNECTING, WebSocket.OPEN, WebSocket.CLOSING, WebSocket.CLOSED]",
    ),
    [0, 1, 2, 3],
    "browser automation changed the native WebSocket API",
  );
  if (values["frame-counter"])
    await evaluate<void>(
      `window.__rustVideoObserver.configureFrameCounter(${JSON.stringify(frameCounterConfiguration)})`,
    );
  ffplayPid = await launchFfplay();
  report.ffplay = { pid: ffplayPid, title };
  await browser("click", "#display");
  await browser(
    "wait",
    "--fn",
    'document.querySelector("#control-status")?.textContent === "Input active"',
  );
  report.warmup = await waitForStableCanvas();
  const output = await remote("wlr-randr", [], { env: runtimeEnvironment });
  assert.match(
    output.stdout.toString(),
    /1824x848 px, 60\.000000 Hz \(current\)/u,
  );
  const topologyBefore = await readTopology();
  report.topologyBefore = topologyBefore;
  report.expectedQuality = { bitrateKbps: 8000, scalePercent: 100 };
  assert.match(
    topologyBefore.ffmpegCommand,
    /(?:^|\s)-b:v 8000k(?:\s|$)/u,
    "fixed-quality recording requires 8000 kbps before recording; current adaptive quality is different",
  );
  const stageTimingPaths = values.stages
    ? await readStageTimingPaths(topologyBefore)
    : undefined;
  stageTopology = values.stages ? topologyBefore : undefined;

  const beforeNative = await captureNative("before");
  const beforeBrowser = await captureBrowser("before");
  report.before = { native: beforeNative, browser: beforeBrowser };

  if (stageTimingPaths) {
    stageStartedNanos = await signalStageTiming(topologyBefore, "USR1");
    stageTimingActive = true;
  }
  if (values.delivery) {
    const info = await browser<{
      pid: number;
      session: string;
      active: boolean;
    }>("session", "info");
    assert.equal(info.session, session);
    assert.equal(info.active, true);
    assert.equal(
      upstreamPorts.length,
      proxyObservers.length,
      "missing owned upstream socket identity",
    );
    tcpSampling = startTcpSampling(upstreamPorts);
    if (tunnel)
      tunnelTcpSampling = startTcpSampling(await tunnel.remotePorts(), tunnel);
    hostSampling = await startHostSampling(info.pid);
    for (const observer of proxyObservers) observer.start();
    deliveryActive = true;
  }
  const started = await evaluate<boolean>(`(() => {
    const panel = document.querySelector('section[aria-label="Comparison recording"]');
    if (!panel) return false;
    const workload = panel.querySelector("select");
    const start = [...panel.querySelectorAll("button")].find((button) => button.textContent === "Record 30 seconds");
    if (!(workload instanceof HTMLSelectElement) || !(start instanceof HTMLButtonElement)) return false;
    workload.value = "continuous motion";
    workload.dispatchEvent(new Event("change", { bubbles: true }));
    window.__rustVideoObserver.start();
    start.click();
    return start.disabled;
  })()`);
  assert(started, "the official comparison recorder did not start");
  const cpuPromise = sampleCpu(topologyBefore, ffplayPid);
  await sleep((recordSeconds + 1) * 1000);
  await finishDelivery();
  let nativeStageTrace: ReturnType<typeof parseStageTrace> | undefined;
  let gatewayStageSummary:
    | ReturnType<typeof summarizeGatewayTimingTrace>
    | undefined;
  if (stageTimingPaths) {
    await signalStageTiming(topologyBefore, "USR2");
    stageTimingActive = false;
    assert(stageStartedNanos !== undefined);
    const bytes = await readStageTimingFile(
      stageTimingPaths.native,
      stageStartedNanos,
    );
    const gatewayBytes = await readGatewayTimingFile(
      stageTimingPaths.gateway,
      stageStartedNanos,
    );
    nativeStageTrace = parseStageTrace(JSON.parse(bytes));
    const gatewayStageTrace = parseGatewayTimingTrace(JSON.parse(gatewayBytes));
    assert.equal(
      nativeStageTrace.pid,
      topologyBefore.daemonPid,
      "stage trace came from another process",
    );
    assert.equal(
      nativeStageTrace.overflow_count,
      0,
      "native stage trace overflowed",
    );
    assert.equal(
      gatewayStageTrace.pid,
      topologyBefore.gatewayPid,
      "gateway trace came from another process",
    );
    assert.equal(
      gatewayStageTrace.overflow_count,
      0,
      "gateway stage trace overflowed",
    );
    await writeFile(
      resolve(outputDirectory, "native-stage-timings.json"),
      bytes,
      { flag: "wx" },
    );
    await writeFile(
      resolve(outputDirectory, "gateway-stage-timings.json"),
      gatewayBytes,
      { flag: "wx" },
    );
    gatewayStageSummary = summarizeGatewayTimingTrace(
      gatewayStageTrace,
      nativeStageTrace.records,
    );
    for (const [name, interval] of Object.entries(
      gatewayStageSummary.intervals,
    )) {
      assert(
        interval.count >= 1000,
        `gateway stage ${name} has too few matched spans`,
      );
    }
    report.gatewayStages = gatewayStageSummary;
    await writeFile(
      resolve(outputDirectory, "gateway-stage-summary.json"),
      `${JSON.stringify(gatewayStageSummary, null, 2)}\n`,
      { flag: "wx" },
    );
  }
  const rawRecordingPath = resolve(outputDirectory, "recording.json");
  await browser(
    "download",
    'section[aria-label="Comparison recording"] button:nth-of-type(3)',
    rawRecordingPath,
  );
  const cpu = await cpuPromise;
  await writeFile(
    resolve(outputDirectory, "cpu.json"),
    `${JSON.stringify(cpu, null, 2)}\n`,
    { flag: "wx" },
  );
  const recording = JSON.parse(
    await readFile(rawRecordingPath, "utf8"),
  ) as Recording;
  const pipeline = parseBrowserTimingTrace(
    await evaluate<unknown>("window.__rustVideoObserver.finish()"),
  );
  await writeFile(
    resolve(outputDirectory, "browser-timings.json"),
    JSON.stringify(pipeline, null, 2) + "\n",
    { flag: "wx" },
  );
  const frameCounter = values["frame-counter"]
    ? parseFrameCounterTrace(
        await evaluate<unknown>(
          "window.__rustVideoObserver.finishFrameCounter()",
        ),
      )
    : undefined;
  if (frameCounter)
    await writeFile(
      resolve(outputDirectory, "frame-counter.json"),
      `${JSON.stringify(frameCounter, null, 2)}\n`,
      { flag: "wx" },
    );
  if (values["exercise-recovery"]) {
    const recovery = await runRecoveryExercise();
    recoveryPassed = recovery.passed;
    report.recovery = recovery;
    await writeFile(
      resolve(outputDirectory, "recovery.json"),
      `${JSON.stringify(recovery, null, 2)}\n`,
      { flag: "wx" },
    );
  } else {
    report.recovery =
      "not requested; pass --exercise-recovery to run the separate post-recording decoder recovery phase";
  }
  if (stageTimingPaths && deliverySnapshot) {
    const analysis = await exec(
      "pnpm",
      [
        "exec",
        "tsx",
        fileURLToPath(new URL("analyze-delivery.ts", import.meta.url)),
        outputDirectory,
        "--proxy",
      ],
      { timeout: 15_000, maxBuffer: 1024 * 1024 },
    );
    await writeFile(
      resolve(outputDirectory, "delivery-analysis.json"),
      analysis.stdout,
      { flag: "wx" },
    );
  }
  assert(
    Object.values(pipeline.overflow).every((count) => count === 0),
    "browser timing observation overflowed",
  );
  assert(pipeline.headers.length > 1, "no sustained video header observations");
  assert(
    pipeline.decodeSubmissions.length > 1,
    "no sustained video decode submissions",
  );
  assert(
    pipeline.decoderOutputs.length > 1,
    "no sustained video decoder outputs",
  );
  const firstHeader = pipeline.headers[0]!;
  const lastHeader = pipeline.headers.at(-1)!;
  const captureSeconds =
    Number(BigInt(lastHeader.captureNanos) - BigInt(firstHeader.captureNanos)) /
    1e9;
  const captureSequenceSpan = Number(
    BigInt(lastHeader.sequence) - BigInt(firstHeader.sequence),
  );
  report.pipeline = {
    browserTimings: summarizeBrowserTimings(pipeline, recording.sdkStats),
    decoderResets: pipeline.resets,
    discontinuities: pipeline.headers.filter(
      (header) => (header.flags & 2) !== 0,
    ).length,
    receivedVideoFrames: pipeline.headers.length,
    receivedVideoFps: pipeline.headers.length / recordSeconds,
    captureSequenceRate: captureSequenceSpan / captureSeconds,
    captureSequencesNotReceived:
      captureSequenceSpan - (pipeline.headers.length - 1),
    scope:
      "Video headers from this browser's stream, without payload. Missing capture sequences do not distinguish encoder replacement from transport or gateway loss.",
  };
  const { evaluation, checks } = recordingEvaluation(recording);
  if (frameCounter) {
    const summary = summarizeFrameCounter(frameCounter, recording.durationMs);
    report.frameCounter = {
      encoding: {
        ...frameCounterConfiguration,
        layout:
          "white/black guards, 16 least-significant-bit-first value/inverse pairs, black/white guards",
      },
      ...summary,
    };
    checks.push({
      name: "source frame counter read for every observed recording draw",
      passed:
        frameCounter.drawHookInstalled &&
        summary.recordedDraws > 0 &&
        summary.recordedDraws === recording.canvasSubmitsMs.length &&
        summary.invalidDraws === 0 &&
        summary.overflow === 0,
      actual: {
        drawHookInstalled: frameCounter.drawHookInstalled,
        recordedDraws: summary.recordedDraws,
        recorderDraws: recording.canvasSubmitsMs.length,
        invalidDraws: summary.invalidDraws,
        overflow: summary.overflow,
      },
      requirement:
        "> 0 draws, exact count match with the recorder, every counter read valid, and 0 overflow",
    });
  } else {
    report.frameCounter =
      "not requested; pass --frame-counter to add and read the probe-only source-frame strip";
  }
  if (nativeStageTrace) {
    assert(gatewayStageSummary, "gateway stage summary is missing");
    const stageSummary = summarizeStageTrace(
      nativeStageTrace,
      pipeline.headers,
    );
    report.nativeStages = stageSummary;
    await writeFile(
      resolve(outputDirectory, "native-stage-summary.json"),
      `${JSON.stringify(stageSummary, null, 2)}\n`,
      { flag: "wx" },
    );
    checks.push(
      {
        name: "native stage trace did not overflow",
        passed: stageSummary.overflowCount === 0,
        actual: stageSummary.overflowCount,
        requirement: "exactly 0 dropped records",
      },
      {
        name: "gateway stages are complete and did not overflow",
        passed:
          gatewayStageSummary.overflowCount === 0 &&
          Object.values(gatewayStageSummary.intervals).every(
            ({ count }) => count >= 1000,
          ),
        actual: gatewayStageSummary,
        requirement:
          "0 dropped records and >= 1000 spans for each gateway interval",
      },
      {
        name: "native stages correlate with delivered browser frames",
        passed: stageSummary.matchedBrowserFrames >= 1000,
        actual: stageSummary.matchedBrowserFrames,
        requirement: ">= 1000 matching sequence and generation pairs",
      },
    );
  }
  checks.push({
    name: "expected quality throughout recording",
    passed: hasExpectedQuality(recording.sdkStats),
    actual: [
      ...new Set(
        recording.sdkStats.map(
          (sample) => `${sample.bitrateKbps} kbps at ${sample.scalePercent}%`,
        ),
      ),
    ],
    requirement: "8000 kbps at 100% scale in every SDK sample",
  });
  checks.push({
    name: "no decoder resets during steady motion",
    passed: pipeline.resets.length === 0,
    actual: pipeline.resets.length,
    requirement: "0 resets",
  });
  if (deliverySnapshot) {
    const { streams, host, observerOverflow } = deliverySnapshot;
    const observation = {
      streams: streams.length,
      frames: streams.reduce(
        (count, stream) => count + stream.records.length,
        0,
      ),
      proxyErrors: streams.flatMap((stream) =>
        stream.error ? [stream.error] : [],
      ),
      proxyOverflow:
        observerOverflow +
        streams.reduce((count, stream) => count + stream.overflow, 0),
      hostSamples: host.samples.length,
      hostErrors: host.errors,
      hostOverflow: host.overflow,
      tcpSamples: deliverySnapshot.tcp?.samples.length ?? 0,
      tcpErrors: deliverySnapshot.tcp?.errors ?? [],
      tcpOverflow: deliverySnapshot.tcp?.overflow ?? 0,
      tcpScope: tunnel
        ? "Node proxy to local CLI listener; loopback only"
        : "Node proxy to public HTTPS endpoint",
      tunnelTcp: deliverySnapshot.tunnelTcp
        ? {
            ownerPid: deliverySnapshot.tunnelTcp.ownerPid,
            samples: deliverySnapshot.tunnelTcp.samples.length,
            errors: deliverySnapshot.tunnelTcp.errors,
            overflow: deliverySnapshot.tunnelTcp.overflow,
            scope:
              "All established remote TCP sockets owned by the CLI at recording start; no assignment to individual video/control connections",
          }
        : null,
    };
    report.delivery = observation;
    checks.push({
      name: "delivery observation is bounded and complete",
      passed:
        observation.frames >= 1000 &&
        observation.proxyErrors.length === 0 &&
        observation.proxyOverflow === 0 &&
        observation.tcpSamples >= 50 &&
        observation.tcpErrors.length === 0 &&
        observation.tcpOverflow === 0 &&
        (!tunnel ||
          (observation.tunnelTcp !== null &&
            observation.tunnelTcp.samples >= 50 &&
            observation.tunnelTcp.errors.length === 0 &&
            observation.tunnelTcp.overflow === 0)) &&
        observation.hostSamples >= 25 &&
        host.errors.length === 0 &&
        Object.values(host.overflow).every((value) => value === 0),
      actual: observation,
      requirement:
        ">= 1000 proxy frames, >= 25 host samples, >= 50 TCP samples, no observation errors or overflow",
    });
  }
  report.recording = evaluation;
  report.checks = checks;
  report.cpu = cpuSummary(cpu);

  const afterNative = await captureNative("after");
  const afterBrowser = await captureBrowser("after");
  assert.notEqual(
    beforeNative.sha256,
    afterNative.sha256,
    "native screenshots did not change during the continuous-motion workload",
  );
  assert.notEqual(
    beforeBrowser.sha256,
    afterBrowser.sha256,
    "browser screenshots did not change during the continuous-motion workload",
  );
  report.after = { native: afterNative, browser: afterBrowser };
  report.contentEvidence = {
    nativeScreenshotsChanged: true,
    browserScreenshotsChanged: true,
    warmupMinimumSampledColors: Math.min(
      ...(report.warmup as CanvasObservation[]).map(({ colors }) => colors),
    ),
    warmupDistinctSampleHashes: new Set(
      (report.warmup as CanvasObservation[]).map(
        ({ sampleHash }) => sampleHash,
      ),
    ).size,
  };

  let fidelityPassed = true;
  if (values.fidelity) {
    await browser("click", "#display");
    await browser(
      "wait",
      "--fn",
      'document.querySelector("#control-status")?.textContent === "Input active"',
    );
    await browser("press", "p");
    await sleep(1000);
    const native = await captureNative("paused");
    const browserImage = await captureBrowser("paused");
    const dataUrl = await evaluate<string>(
      'document.querySelector("#display").toDataURL("image/png")',
    );
    assert(dataUrl.startsWith("data:image/png;base64,"));
    const decodedPath = resolve(outputDirectory, "paused-decoded.png");
    await writeFile(
      decodedPath,
      Buffer.from(dataUrl.slice("data:image/png;base64,".length), "base64"),
      { flag: "wx" },
    );
    const stillNative = await captureNative("paused-still");
    assert.equal(
      stillNative.sha256,
      native.sha256,
      "native application did not remain paused during fidelity capture",
    );
    const quality = await exec(
      "ffmpeg",
      [
        "-hide_banner",
        "-nostdin",
        "-i",
        native.path,
        "-i",
        decodedPath,
        "-lavfi",
        "[0:v]format=gbrp[a];[1:v]format=gbrp[b];[a][b]psnr",
        "-frames:v",
        "1",
        "-f",
        "null",
        "-",
      ],
      { timeout: 15_000 },
    );
    await writeFile(
      resolve(outputDirectory, "fidelity-psnr.log"),
      quality.stderr,
      { flag: "wx" },
    );
    const match = /average:([\d.]+|inf)/u.exec(quality.stderr);
    assert(match, "FFmpeg did not report RGB PSNR");
    const rgbPsnrDb = match[1] === "inf" ? Infinity : Number(match[1]);
    fidelityPassed = rgbPsnrDb >= 35;
    checks.push({
      name: "paused-frame RGB fidelity",
      passed: fidelityPassed,
      actual: rgbPsnrDb,
      requirement: ">= 35 dB PSNR; native screenshots identical",
    });
    report.fidelity = {
      scope:
        "Browser p key paused the native application. Two identical native screenshots bracket the decoded-canvas capture. RGB PSNR measures this static test chart, not all desktop text or motion quality.",
      native,
      stillNative,
      browser: browserImage,
      decodedPath,
      rgbPsnrDb: Number.isFinite(rgbPsnrDb) ? rgbPsnrDb : "identical",
    };
  } else {
    report.fidelity =
      "not requested; pass --fidelity to capture a real browser-paused frame pair";
  }

  tunnel?.assertAlive();
  const topologyAfter = await readTopology();
  report.topologyAfter = topologyAfter;
  assert.deepEqual(
    topologyAfter,
    topologyBefore,
    "baseline service topology changed during the run",
  );
  gatePassed =
    checks.every((check) => check.passed) && fidelityPassed && recoveryPassed;
  report.status = gatePassed ? "PASSED" : "FAILED";
} catch (error) {
  primaryError = error;
  report.error = error instanceof Error ? error.message : String(error);
} finally {
  try {
    await finishDelivery();
    await tcpSampling?.finish();
    await tunnelTcpSampling?.finish();
    await hostSampling?.finish();
  } catch (error) {
    cleanup.deliveryError =
      error instanceof Error ? error.message : String(error);
    if (!primaryError) primaryError = error;
  }
  if (stageTimingActive && stageTopology && sprite) {
    try {
      await signalStageTiming(stageTopology, "USR2");
      stageTimingActive = false;
      cleanup.stageTimings = "stopped after probe error";
    } catch (error) {
      cleanup.stageTimingStopError =
        error instanceof Error ? error.message : String(error);
      if (!primaryError) primaryError = error;
    }
  }
  if (browserOpened) {
    try {
      await browser("close");
      cleanup.browserClosed = true;
    } catch (error) {
      cleanup.browserCloseError =
        error instanceof Error ? error.message : String(error);
      if (!primaryError) primaryError = error;
    }
  }
  if (proxy) {
    try {
      proxy.revokeCredential();
      proxy.server.closeAllConnections();
      if (proxy.server.listening)
        await new Promise<void>((resolveClose, reject) => {
          const timer = setTimeout(
            () =>
              reject(
                new Error("acceptance proxy did not close within 5 seconds"),
              ),
            5000,
          );
          proxy!.server.close((error) => {
            clearTimeout(timer);
            if (error) reject(error);
            else resolveClose();
          });
        });
      cleanup.proxyClosed = true;
    } catch (error) {
      cleanup.proxyCloseError =
        error instanceof Error ? error.message : String(error);
      if (!primaryError) primaryError = error;
    }
  }
  if (tunnel) {
    try {
      await tunnel.close();
      cleanup.tunnelClosed = true;
    } catch (error) {
      cleanup.tunnelCloseError =
        error instanceof Error ? error.message : String(error);
      if (!primaryError) primaryError = error;
    }
  }
  if (sprite && ffplayPid) {
    try {
      cleanup.ffplay = await stopOwnedFfplay(ffplayPid);
    } catch (error) {
      cleanup.ffplay = {
        status: "cleanup failed",
        error: error instanceof Error ? error.message : String(error),
      };
      if (!primaryError) primaryError = error;
    }
  }
  if (sprite) {
    for (const path of remoteCaptures) {
      try {
        await sprite.filesystem("/").rm(path, { force: true });
      } catch (error) {
        cleanup.remoteCaptureRemovalError =
          error instanceof Error ? error.message : String(error);
      }
    }
    try {
      await sprite.filesystem("/").rm(remoteLog, { force: true });
      cleanup.remoteLogRemoved = true;
    } catch (error) {
      cleanup.remoteLogRemovalError =
        error instanceof Error ? error.message : String(error);
    }
  }
  if (primaryError) {
    report.status = "FAILED";
    report.error =
      primaryError instanceof Error
        ? primaryError.message
        : String(primaryError);
  }
  report.cleanup = cleanup;
  report.endedAt = new Date().toISOString();
  await writeFile(
    resolve(outputDirectory, "summary.json"),
    `${JSON.stringify(report, null, 2)}\n`,
    { flag: "wx" },
  );
}

console.log(JSON.stringify(report, null, 2));
if (primaryError) throw primaryError;
if (!gatePassed)
  throw new Error(
    `performance gate failed; evidence is preserved in ${outputDirectory}`,
  );
