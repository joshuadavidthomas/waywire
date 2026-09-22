import { WaywireSession } from "../../../gateway/web/src/sdk/waywire.ts";
import { ClockSynchronizer } from "../../../gateway/web/src/sdk/control.ts";
import {
  Playout,
  initialPlayoutTargetMs,
} from "../../../gateway/web/src/sdk/playout.ts";
import {
  decodeRecord,
  readFrameMetadata,
} from "../../../gateway/web/src/sdk/wire.ts";
import { decodeMarker } from "./marker.js";

const canvas = document.querySelector("canvas");
const status = document.querySelector("#status");
const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));
const fixedTarget = new URL(location.href).searchParams.get("latencyTargetMs");
const fixedTargetMs = fixedTarget === null ? null : Number(fixedTarget);
if (
  fixedTarget !== null &&
  (fixedTarget.trim() === "" ||
    !Number.isFinite(fixedTargetMs) ||
    fixedTargetMs < 0)
)
  throw new RangeError("latencyTargetMs must be a non-negative number");
const playoutMode =
  fixedTargetMs === null
    ? { mode: "adaptive" }
    : { mode: "fixed", targetMs: fixedTargetMs };
let session,
  current,
  pending,
  nextMarker = 0,
  lastMarker = null,
  ticker;
let lastRaf = 0,
  recording = false,
  extraReads = 0;
const results = {
  schema: 1,
  userAgent: navigator.userAgent,
  hardwareConcurrency: navigator.hardwareConcurrency,
  devicePixelRatio,
  timeOrigin: performance.timeOrigin,
  playout: playoutMode,
  blocks: [],
};
// Study-only observation of the actual filter, not a second clock estimate.
// These private field reads are pinned to this SDK source; no policy is changed.
const updateClock = ClockSynchronizer.prototype.update;
ClockSynchronizer.prototype.update = function (sent, received, serverNanos) {
  const previous = Reflect.get(this, "lastSampleAt");
  updateClock.call(this, sent, received, serverNanos);
  current?.clockSamples.push({
    atMs: performance.now(),
    sentMs: sent,
    receivedMs: received,
    rttMs: received - sent,
    accepted: Reflect.get(this, "lastSampleAt") !== previous,
    confident: this.synchronized(),
    bestRttMs: this.bestRttMilliseconds,
    offsetMicros: this.offsetMicros,
  });
};
function raf(now) {
  if (recording && lastRaf) current.rafIntervals.push(now - lastRaf);
  lastRaf = now;
  requestAnimationFrame(raf);
}
requestAnimationFrame(raf);
const plain = (value) =>
  JSON.parse(
    JSON.stringify(value, (_, v) => (typeof v === "bigint" ? String(v) : v)),
  );
async function until(test, timeout = 15000) {
  const deadline = performance.now() + timeout;
  while (!test()) {
    if (performance.now() >= deadline) throw new Error("Readiness timeout");
    await sleep(25);
  }
}
function readMarker() {
  const context = canvas.getContext("2d");
  const sx = canvas.width / 1920,
    sy = canvas.height / 1080;
  if (!sx || !sy) return null;
  const rows = [80, 112].map(
    (y) =>
      context.getImageData(
        Math.round(32 * sx),
        Math.round(y * sy),
        Math.ceil(544 * sx),
        1,
      ).data,
  );
  return decodeMarker(rows, sx);
}
function complete(value) {
  const active = pending;
  pending = null;
  clearTimeout(active.timer);
  active.resolve({
    expected: active.expected,
    startedAtMs: active.start,
    sequence: active.sequence,
    ackAtMs: active.ackAtMs,
    ...value,
  });
}

async function configure(
  fps,
  mode,
  playout = playoutMode,
  videoJitter = false,
) {
  const blockTargetMs = playout.mode === "fixed" ? playout.targetMs : null;
  if (
    playout.mode !== "adaptive" &&
    (playout.mode !== "fixed" ||
      !Number.isFinite(blockTargetMs) ||
      blockTargetMs < 0)
  )
    throw new RangeError("Invalid block playout mode");
  recording = false;
  if (session) await session.dispose();
  clearInterval(ticker);
  const response = await fetch("/api/condition", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify({ fps, mode, videoJitter }),
  });
  if (!response.ok) throw new Error(await response.text());
  current = {
    configuration: { ...(await response.json()), playout },
    clockSamples: [],
    quality: [],
    metadata: [],
    states: [],
    frames: [],
    responses: [],
    rafIntervals: [],
    startedAtMs: performance.now(),
  };
  results.blocks.push(current);
  lastMarker = null;
  nextMarker = 0;
  const adaptive = new Playout();
  let target = adaptive.update(null, performance.now());
  const updatePlayout = (sample) => {
    if (blockTargetMs !== null) return;
    const next = adaptive.update(sample, performance.now());
    if (next !== target) {
      target = next;
      session.video.setLatencyTarget(target);
    }
  };
  let lastConfiguration = "";
  session = new WaywireSession({
    latency: blockTargetMs ?? initialPlayoutTargetMs,
    statsIntervalMs: 0,
    createWebSocket(path, url) {
      const socket = new WebSocket(url);
      socket.binaryType = "arraybuffer";
      const send = socket.send.bind(socket);
      socket.send = (data) => {
        if (pending && data instanceof ArrayBuffer) {
          const { kind, payload } = decodeRecord(data);
          if (kind === 4) {
            payload.u32();
            if (payload.u8() === 1) pending.sequence = payload.u32();
          }
        }
        send(data);
      };
      if (String(path).includes("stream"))
        socket.addEventListener("message", (event) => {
          if (!(event.data instanceof ArrayBuffer)) return;
          const { kind, payload } = decodeRecord(event.data);
          if (kind !== 1) return;
          payload.u8();
          payload.u8();
          const metadata = readFrameMetadata(payload);
          if (
            pending &&
            pending.sequence &&
            metadata.inputSequence >= pending.sequence &&
            pending.ackAtMs === null
          )
            pending.ackAtMs = performance.now();
          const key = JSON.stringify([
            metadata.generation,
            metadata.width,
            metadata.height,
            metadata.fps,
            metadata.chroma,
          ]);
          if (key !== lastConfiguration) {
            current.metadata.push({
              atMs: performance.now(),
              ...plain(metadata),
            });
            lastConfiguration = key;
          }
        });
      return socket;
    },
  });
  session.attachSurface({
    canvas,
    inputElement: canvas,
    controlOnFocus: false,
    clipboardAutoSync: false,
  });
  session.on("state", (state) =>
    current.states.push({ atMs: performance.now(), ...state }),
  );
  session.on("quality", (quality) =>
    current.quality.push({ atMs: performance.now(), ...quality }),
  );
  session.on("stats", (stats) => {
    const start = performance.now();
    const marker = readMarker();
    for (let i = 0; i < extraReads; i++) readMarker();
    const readbackMs = performance.now() - start;
    lastMarker = marker;
    if (recording) current.frames.push({ ...stats, marker, readbackMs });
    if (
      pending &&
      marker === pending.expected &&
      stats.drawCompletedAtMs >= pending.start
    ) {
      complete({
        ok: true,
        marker,
        latencyMs: stats.drawCompletedAtMs - pending.start,
        readbackMs,
        stats,
      });
    }
    updatePlayout(
      stats.clockConfident
        ? { latenessMs: stats.latenessMs, decodeQueue: stats.decoderQueue }
        : null,
    );
  });
  if (blockTargetMs === null)
    ticker = setInterval(() => updatePlayout(null), 1000);
  session.connect();
  await until(() => session.state.input.connected && lastMarker === 0);
  session.input.acquire();
  await until(() => session.state.input.state === "active");
  canvas.focus();
  status.textContent = `Ready: ${fps} FPS ${mode}. Native marker 0 verified.`;
  return {
    state: session.state,
    stats: session.stats,
    metadata: current.metadata,
  };
}

function input({ delayed = false, timeout = 3000 } = {}) {
  if (pending || session.state.input.state !== "active")
    throw new Error("Input is not ready");
  const expected = ++nextMarker;
  if (expected >= 65536)
    throw new Error("Marker space exhausted; restart condition");
  const down = new KeyboardEvent("keydown", {
    code: delayed ? "F9" : "F8",
    key: delayed ? "F9" : "F8",
    bubbles: true,
    cancelable: true,
  });
  const up = new KeyboardEvent("keyup", {
    code: down.code,
    key: down.key,
    bubbles: true,
    cancelable: true,
  });
  return new Promise((resolve) => {
    pending = {
      expected,
      resolve,
      sequence: null,
      ackAtMs: null,
      start: performance.now(),
    };
    pending.timer = setTimeout(
      () => complete({ ok: false, timeout, lastMarker, stats: session.stats }),
      timeout,
    );
    // Same performance.now clock as production drawCompletedAtMs; dispatch invokes
    // the unmodified SDK physical-key handler synchronously, not the ack path.
    canvas.dispatchEvent(down);
    canvas.dispatchEvent(up);
  });
}

async function correctness() {
  await configure(60, "motion");
  await sleep(2000);
  recording = true;
  const delayed = await input({ delayed: true });
  if (
    !delayed.ok ||
    delayed.latencyMs < 350 ||
    delayed.ackAtMs === null ||
    delayed.ackAtMs >= delayed.startedAtMs + delayed.latencyMs - 100
  )
    throw new Error(
      `Delayed marker/ack distinction failed: ${JSON.stringify(delayed)}`,
    );
  const timedOut = await input({ delayed: true, timeout: 50 });
  if (timedOut.ok) throw new Error("Stale pixels accepted as a new response");
  const resync = await input();
  if (!resync.ok || resync.marker !== 3)
    throw new Error("Timeout resynchronization failed");
  await sleep(450);
  if (lastMarker !== 3)
    throw new Error("Delayed stale response overwrote the new response");
  recording = false;
  results.correctness = { delayed, timedOut, resync, finalMarker: lastMarker };
  return results.correctness;
}

async function runBlock(
  fps,
  mode,
  count = 20,
  warmupMs = 12000,
  reads = 0,
  playout = playoutMode,
  videoJitter = false,
) {
  await configure(fps, mode, playout, videoJitter);
  extraReads = reads;
  await sleep(warmupMs);
  current.extraReads = reads;
  current.measurementStartedAtMs = performance.now();
  current.startStats = plain(session.stats);
  recording = true;
  for (let i = 0; i < count; i++) {
    // Independent timers randomize phase instead of dispatching on an rAF edge.
    const random = crypto.getRandomValues(new Uint32Array(1))[0] / 2 ** 32;
    const phaseDelayMs = 70 + 170 * random;
    await sleep(phaseDelayMs);
    const response = await input();
    current.responses.push({ phaseDelayMs, ...response });
    if (!response.ok) {
      const resync = await input();
      current.responses.push({ resync: true, ...resync });
      if (!resync.ok)
        throw new Error(
          "Cannot resynchronize; stop rather than mislabel responses",
        );
    }
    status.textContent = `${fps} ${mode}: ${i + 1}/${count}; marker ${lastMarker}; latest ${response.latencyMs?.toFixed(1) ?? "timeout"} ms`;
  }
  recording = false;
  current.measurementEndedAtMs = performance.now();
  current.endStats = plain(session.stats);
  return {
    fps,
    mode,
    count,
    successful: current.responses.filter((r) => r.ok && !r.resync).length,
    metadata: current.metadata,
  };
}

async function save(name = "results") {
  const saved = await (
    await fetch(`/api/results?name=${encodeURIComponent(name)}`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(results),
    })
  ).json();
  if (current) Object.assign(current, saved.details);
  return saved;
}
async function runSuite() {
  // Three interleaved blocks, 60 independent responses per condition. Rotation
  // balances order without pretending runs on other orbs are paired baselines.
  const order = [
    [60, "quiet"],
    [90, "motion"],
    [120, "quiet"],
    [60, "motion"],
    [90, "quiet"],
    [120, "motion"],
  ];
  for (let block = 0; block < 3; block++) {
    for (let index = 0; index < 6; index++) {
      const [fps, mode] = order[(index + block * 2) % 6];
      await runBlock(fps, mode);
      await save();
    }
  }
  // Sensitivity check: amplify synchronous readback cost fivefold, same policy.
  await runBlock(60, "motion", 20, 12000, 4);
  await save();
  status.textContent =
    "Suite complete. Raw results saved on sender; latency.export() returns them.";
  return { blocks: results.blocks.length, saved: await save() };
}
async function runPlaybackSuite({
  repeats = 3,
  count = 10,
  warmupMs = 12000,
  name = "playback-natural",
} = {}) {
  results.playout = { mode: "per-block" };
  const order = [
    [null, "quiet"],
    [0, "motion"],
    [25, "quiet"],
    [50, "motion"],
    [100, "quiet"],
    [null, "motion"],
    [0, "quiet"],
    [25, "motion"],
    [50, "quiet"],
    [100, "motion"],
  ];
  for (let repeat = 0; repeat < repeats; repeat++) {
    for (let i = 0; i < order.length; i++) {
      const [targetMs, mode] = order[(i + repeat * 3) % order.length];
      const playout =
        targetMs === null ? { mode: "adaptive" } : { mode: "fixed", targetMs };
      await runBlock(60, mode, count, warmupMs, 0, playout);
      await save(name);
    }
  }
  status.textContent =
    "Playback suite complete. Per-block targets and clock samples saved.";
  return { blocks: results.blocks.length, saved: await save(name) };
}
async function runJitterSuite({
  targetMs = 50,
  repeats = 3,
  count = 10,
  warmupMs = 12000,
  name = "playback-jitter",
} = {}) {
  results.playout = { mode: "per-block" };
  for (let repeat = 0; repeat < repeats; repeat++) {
    for (const target of repeat % 2 ? [targetMs, 100] : [100, targetMs]) {
      await runBlock(
        60,
        "motion",
        count,
        warmupMs,
        0,
        { mode: "fixed", targetMs: target },
        true,
      );
      await save(name);
    }
  }
  status.textContent =
    "Jitter suite complete. Artificial video-only pauses recorded.";
  return { blocks: results.blocks.length, saved: await save(name) };
}
window.latency = {
  configure,
  input,
  correctness,
  runBlock,
  runSuite,
  runPlaybackSuite,
  runJitterSuite,
  save,
  export: () => results,
  state: () => ({
    state: session?.state,
    stats: session?.stats,
    marker: lastMarker,
    pending: pending?.expected,
  }),
  stop: async () => {
    recording = false;
    clearInterval(ticker);
    await session?.dispose();
  },
};
