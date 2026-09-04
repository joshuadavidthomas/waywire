import assert from "node:assert/strict";
import { spawn, type ChildProcess } from "node:child_process";
import { access, mkdtemp, readFile, rm } from "node:fs/promises";
import { createConnection, createServer, type Server } from "node:net";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";
import WebSocket, { type ClientOptions, type RawData } from "ws";

const TEST_TIMEOUT_MS = 8_000;
const GATEWAY_CONNECTION_LIMIT = 32;
const origin = "https://gateway.test";
const fixtureDirectory = join(
  dirname(fileURLToPath(import.meta.url)),
  "fixtures",
);
const peer = join(fixtureDirectory, "streamd-peer.ts");

interface GoldenWire {
  source: string;
  releaseAll: string;
  keyframeReadyGeneration4: string;
  videoHeaderSequence17: string;
}

interface ExitStatus {
  code: number | null;
  signal: NodeJS.Signals | null;
}

interface SocketMessage {
  data: Buffer;
  binary: boolean;
}

function gatewayArgument(): string {
  const index = process.argv.indexOf("--gateway");
  if (index < 0 || !process.argv[index + 1]) {
    throw new Error("usage: ipc-check.ts --gateway PATH");
  }
  return resolve(process.argv[index + 1]!);
}

function delay(milliseconds: number): Promise<void> {
  return new Promise((done) => setTimeout(done, milliseconds));
}

async function bounded<T>(
  operation: Promise<T>,
  label: string,
  milliseconds = TEST_TIMEOUT_MS,
): Promise<T> {
  let timer: NodeJS.Timeout | undefined;
  try {
    return await Promise.race([
      operation,
      new Promise<never>((_, reject) => {
        timer = setTimeout(
          () => reject(new Error(`timed out waiting for ${label}`)),
          milliseconds,
        );
      }),
    ]);
  } finally {
    if (timer) clearTimeout(timer);
  }
}

async function waitFor(
  check: () => Promise<boolean>,
  label: string,
  milliseconds = TEST_TIMEOUT_MS,
): Promise<void> {
  const end = Date.now() + milliseconds;
  while (Date.now() < end) {
    if (await check().catch(() => false)) return;
    await delay(25);
  }
  throw new Error(`timed out waiting for ${label}`);
}

async function freePort(): Promise<number> {
  const server = createServer();
  await bounded(
    new Promise<void>((done, fail) => {
      server.once("error", fail);
      server.listen(0, "127.0.0.1", done);
    }),
    "test port allocation",
  );
  const address = server.address();
  if (!address || typeof address === "string") {
    throw new Error("could not allocate a TCP test port");
  }
  await closeServer(server);
  return address.port;
}

async function closeServer(server: Server): Promise<void> {
  if (!server.listening) return;
  await bounded(
    new Promise<void>((done, fail) =>
      server.close((error) => (error ? fail(error) : done())),
    ),
    "TCP listener close",
  );
}

function processExists(pid: number): boolean {
  try {
    process.kill(pid, 0);
    return true;
  } catch (error) {
    return (error as NodeJS.ErrnoException).code !== "ESRCH";
  }
}

async function stopOwnedPid(pid: number): Promise<void> {
  if (!processExists(pid)) return;
  process.kill(pid, "SIGTERM");
  await waitFor(
    async () => !processExists(pid),
    `owned PID ${pid} exit`,
    750,
  ).catch(() => {});
  if (processExists(pid)) {
    process.kill(pid, "SIGKILL");
    await waitFor(
      async () => !processExists(pid),
      `owned PID ${pid} kill`,
      750,
    ).catch(() => {});
  }
}

async function readPid(path: string): Promise<number | undefined> {
  const value = Number((await readFile(path, "utf8").catch(() => "")).trim());
  return Number.isInteger(value) && value > 0 ? value : undefined;
}

class GatewayRun {
  readonly child: ChildProcess;
  readonly port: number;
  readonly commandLog: string;
  readonly stateLog: string;
  readonly peerPidFile: string;
  readonly descendantPidFile: string;
  readonly directory: string;
  readonly exit: Promise<ExitStatus>;
  #diagnostics = "";
  #cleaned = false;

  private constructor(
    child: ChildProcess,
    port: number,
    directory: string,
    commandLog: string,
    stateLog: string,
    peerPidFile: string,
    descendantPidFile: string,
  ) {
    this.child = child;
    this.port = port;
    this.directory = directory;
    this.commandLog = commandLog;
    this.stateLog = stateLog;
    this.peerPidFile = peerPidFile;
    this.descendantPidFile = descendantPidFile;
    this.exit = new Promise((done, fail) => {
      child.once("error", fail);
      child.once("exit", (code, signal) => done({ code, signal }));
    });
    child.stderr?.on("data", (part: Buffer) => {
      this.#diagnostics = (this.#diagnostics + part.toString()).slice(-65_536);
    });
  }

  static async start(
    gateway: string,
    mode: string,
    options: { port?: number; env?: NodeJS.ProcessEnv } = {},
  ): Promise<GatewayRun> {
    const directory = await mkdtemp(join(tmpdir(), "sprite-gateway-"));
    const port = options.port ?? (await freePort());
    const commandLog = join(directory, "commands.log");
    const stateLog = join(directory, "state.log");
    const peerPidFile = join(directory, "peer.pid");
    const descendantPidFile = join(directory, "descendant.pid");
    const child = spawn(
      gateway,
      [
        "--listen",
        `127.0.0.1:${port}`,
        "--streamd",
        peer,
        "--public-url",
        origin,
      ],
      {
        env: {
          ...process.env,
          SPRITE_GATEWAY_FIXTURE_MODE: mode,
          SPRITE_GATEWAY_COMMAND_LOG: commandLog,
          SPRITE_GATEWAY_STATE_LOG: stateLog,
          SPRITE_GATEWAY_PEER_PID: peerPidFile,
          SPRITE_GATEWAY_DESCENDANT_PID: descendantPidFile,
          ...options.env,
        },
        stdio: ["ignore", "ignore", "pipe"],
      },
    );
    const run = new GatewayRun(
      child,
      port,
      directory,
      commandLog,
      stateLog,
      peerPidFile,
      descendantPidFile,
    );
    activeRuns.add(run);
    return run;
  }

  diagnostics(): string {
    return this.#diagnostics;
  }

  async waitForExit(milliseconds = TEST_TIMEOUT_MS): Promise<ExitStatus> {
    return await bounded(this.exit, "gateway exit", milliseconds);
  }

  async terminate(): Promise<ExitStatus> {
    if (this.child.exitCode === null && this.child.signalCode === null) {
      this.child.kill("SIGTERM");
    }
    return await this.waitForExit(3_000).catch(async () => {
      if (this.child.pid) await stopOwnedPid(this.child.pid);
      return await this.waitForExit(2_000);
    });
  }

  async cleanup(): Promise<void> {
    if (this.#cleaned) return;
    this.#cleaned = true;
    await this.terminate().catch(() => {});
    const owned = await Promise.all([
      readPid(this.peerPidFile),
      readPid(this.descendantPidFile),
    ]);
    for (const pid of owned) {
      if (pid) await stopOwnedPid(pid);
    }
    await rm(this.directory, { recursive: true, force: true });
    activeRuns.delete(this);
  }
}

class SocketInbox {
  readonly socket: WebSocket;
  #queue: SocketMessage[] = [];
  #waiter:
    | {
        done: (message: SocketMessage) => void;
        fail: (error: Error) => void;
      }
    | undefined;
  #failure: Error | undefined;
  #discard = false;

  constructor(socket: WebSocket) {
    this.socket = socket;
    socket.on("message", (data: RawData, binary: boolean) => {
      if (this.#discard) return;
      const message = { data: Buffer.from(data as Uint8Array), binary };
      if (this.#waiter) {
        const waiter = this.#waiter;
        this.#waiter = undefined;
        waiter.done(message);
        return;
      }
      if (this.#queue.length === 256) {
        this.#fail(
          new Error("WebSocket fixture message queue exceeded 256 records"),
        );
        socket.terminate();
        return;
      }
      this.#queue.push(message);
    });
    socket.on("error", (error) => this.#fail(error));
    socket.on("close", () => this.#fail(new Error("WebSocket closed")));
  }

  #fail(error: Error): void {
    this.#failure ??= error;
    if (this.#waiter) {
      const waiter = this.#waiter;
      this.#waiter = undefined;
      waiter.fail(error);
    }
  }

  async next(label: string): Promise<SocketMessage> {
    if (this.#queue.length > 0) return this.#queue.shift()!;
    if (this.#failure) throw this.#failure;
    return await bounded(
      new Promise<SocketMessage>((done, fail) => {
        this.#waiter = { done, fail };
      }),
      label,
    );
  }

  discardMessages(): void {
    this.#queue = [];
    this.#discard = true;
  }
}

const activeRuns = new Set<GatewayRun>();
const activeSockets = new Set<WebSocket>();

async function openSocket(
  url: string,
  options: ClientOptions = { origin },
): Promise<SocketInbox> {
  const socket = new WebSocket(url, options);
  activeSockets.add(socket);
  const inbox = new SocketInbox(socket);
  await bounded(
    new Promise<void>((done, fail) => {
      socket.once("open", done);
      socket.once("error", fail);
    }),
    `WebSocket open ${url}`,
  );
  socket.once("close", () => activeSockets.delete(socket));
  return inbox;
}

async function rejectSocket(
  url: string,
  options: ClientOptions,
): Promise<void> {
  const socket = new WebSocket(url, options);
  activeSockets.add(socket);
  await bounded(
    new Promise<void>((done, fail) => {
      let settled = false;
      const finish = (error?: Error): void => {
        if (settled) return;
        settled = true;
        activeSockets.delete(socket);
        if (error) fail(error);
        else done();
      };
      socket.once("open", () =>
        finish(new Error("forbidden Origin was accepted")),
      );
      socket.once("unexpected-response", (_request, response) => {
        response.resume();
        finish(
          response.statusCode === 403
            ? undefined
            : new Error(`Origin rejection returned ${response.statusCode}`),
        );
      });
      socket.once("error", () => {});
    }),
    "WebSocket Origin rejection",
  );
  socket.terminate();
}

async function rejectSocketStatus(url: string, status: number): Promise<void> {
  const socket = new WebSocket(url, { origin });
  activeSockets.add(socket);
  await bounded(
    new Promise<void>((done, fail) => {
      let settled = false;
      const finish = (error?: Error): void => {
        if (settled) return;
        settled = true;
        activeSockets.delete(socket);
        if (error) fail(error);
        else done();
      };
      socket.once("open", () =>
        finish(new Error(`HTTP ${status} WebSocket rejection was accepted`)),
      );
      socket.once("unexpected-response", (_request, response) => {
        response.resume();
        finish(
          response.statusCode === status
            ? undefined
            : new Error(
                `WebSocket rejection returned ${response.statusCode}, expected ${status}`,
              ),
        );
      });
      socket.once("error", () => {});
    }),
    `WebSocket HTTP ${status} rejection`,
  );
  socket.terminate();
}

async function rejectDuplicateOrigin(url: string): Promise<void> {
  const target = new URL(url);
  const socket = createConnection({
    host: target.hostname,
    port: Number(target.port),
  });
  try {
    await bounded(
      new Promise<void>((done, fail) => {
        socket.once("connect", done);
        socket.once("error", fail);
      }),
      "duplicate-Origin TCP connection",
    );
    socket.write(
      [
        `GET ${target.pathname} HTTP/1.1`,
        `Host: ${target.host}`,
        "Connection: Upgrade",
        "Upgrade: websocket",
        "Sec-WebSocket-Version: 13",
        "Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==",
        `Origin: ${origin}`,
        `Origin: ${origin}`,
        "",
        "",
      ].join("\r\n"),
    );
    const response = await bounded(
      new Promise<Buffer>((done, fail) => {
        socket.once("data", done);
        socket.once("error", fail);
      }),
      "duplicate-Origin rejection",
    );
    assert.match(response.toString(), /^HTTP\/1\.1 403 /);
  } finally {
    socket.destroy();
  }
}

function waitForSocketClose(socket: WebSocket): Promise<void> {
  if (socket.readyState === WebSocket.CLOSED) return Promise.resolve();
  return new Promise<void>((done) => socket.once("close", () => done()));
}

async function closeSocket(inbox: SocketInbox): Promise<void> {
  const socket = inbox.socket;
  if (socket.readyState === WebSocket.CLOSED) return;
  const closed = new Promise<void>((done) =>
    socket.once("close", () => done()),
  );
  socket.close();
  await bounded(closed, "WebSocket close", 1_000).catch(() =>
    socket.terminate(),
  );
  activeSockets.delete(socket);
}

async function send(socket: WebSocket, value: string | Buffer): Promise<void> {
  await bounded(
    new Promise<void>((done, fail) =>
      socket.send(value, (error) => (error ? fail(error) : done())),
    ),
    "WebSocket send",
  );
}

async function nextJson(
  inbox: SocketInbox,
  type: string,
): Promise<Record<string, unknown>> {
  for (let count = 0; count < 32; count += 1) {
    const message = await inbox.next(`${type} WebSocket message`);
    if (message.binary) continue;
    const value = JSON.parse(message.data.toString()) as Record<
      string,
      unknown
    >;
    if (value.type === type) return value;
  }
  throw new Error(`did not receive ${type} within 32 messages`);
}

async function fetchStatus(url: string): Promise<number> {
  const response = await fetch(url, { signal: AbortSignal.timeout(1_000) });
  await response.body?.cancel();
  return response.status;
}

async function fileContains(path: string, value: string): Promise<boolean> {
  return (await readFile(path, "utf8").catch(() => "")).includes(value);
}

function keyboard(sequence: number): Buffer {
  const value = Buffer.alloc(16);
  value.set([2, 4, 1, 0]);
  value.writeUInt32LE(30, 4);
  value.writeUInt32LE(sequence, 12);
  return value;
}

async function assertOwnedDescendantGone(run: GatewayRun): Promise<void> {
  const pid = await readPid(run.descendantPidFile);
  assert(pid, "fixture did not record its owned descendant");
  await waitFor(
    async () => !processExists(pid),
    `owned descendant ${pid} cleanup`,
    3_000,
  );
}

async function checkLiveGateway(
  gateway: string,
  wire: GoldenWire,
): Promise<void> {
  const run = await GatewayRun.start(gateway, "live", {
    env: {
      SPRITE_GATEWAY_FIXTURE_METADATA_DELAY_MS: "100",
      SPRITE_GATEWAY_FIXTURE_RTP_DELAY_MS: "600",
    },
  });
  const base = `http://127.0.0.1:${run.port}`;
  const wsBase = `ws://127.0.0.1:${run.port}`;
  let stream: SocketInbox | undefined;
  let first: SocketInbox | undefined;
  let second: SocketInbox | undefined;
  try {
    await waitFor(
      async () => (await fetchStatus(`${base}/healthz`)) === 503,
      "HTTP server before media readiness",
    );
    stream = await openSocket(`${wsBase}/stream`);
    const configuration = await nextJson(stream, "video-config");
    assert.equal(configuration.version, 2);
    assert.equal(configuration.codec, "avc1.F40034");

    await waitFor(
      () => fileContains(run.stateLog, "metadata\n"),
      "fragmented metadata event",
    );
    assert.equal(
      await fetchStatus(`${base}/healthz`),
      503,
      "metadata without a keyframe must not mark the gateway ready",
    );
    await waitFor(
      async () => (await fetchStatus(`${base}/healthz`)) === 200,
      "keyframe readiness",
    );
    const frame = await stream.next("video frame completed by RTP marker");
    assert(frame.binary, "stream frame was not binary");
    assert.equal(
      frame.data.subarray(0, 40).toString("hex"),
      wire.videoHeaderSequence17,
      "gateway video header diverged from the source-verified golden fixture",
    );
    assert.deepEqual([...frame.data.subarray(40)], [0, 0, 0, 1, 0x65, 0x88]);

    const asset = await fetch(`${base}/`, {
      signal: AbortSignal.timeout(1_000),
    });
    assert.equal(asset.status, 200);
    assert.equal(asset.headers.get("cache-control"), "no-store");
    await asset.body?.cancel();

    await rejectSocket(`${wsBase}/control`, {});
    await rejectSocket(`${wsBase}/control`, { origin: "null" });
    await rejectSocket(`${wsBase}/control`, { origin: "https://foreign.test" });
    await rejectDuplicateOrigin(`${wsBase}/control`);

    first = await openSocket(`${wsBase}/control`);
    await nextJson(first, "cursor");
    assert.deepEqual(await nextJson(first, "quality"), {
      type: "quality",
      bitrate: 8_000,
      fps: 60,
      scale: 100,
    });
    await send(first.socket, "acquire");
    assert.equal((await nextJson(first, "control-state")).state, "active");
    await send(first.socket, keyboard(1));
    await waitFor(
      () => fileContains(run.commandLog, `${keyboard(1).toString("hex")}\n`),
      "first key reaching the native peer before release",
    );
    await send(first.socket, "release");
    assert.equal((await nextJson(first, "control-state")).state, "ready");

    await send(first.socket, "acquire");
    assert.equal((await nextJson(first, "control-state")).state, "active");
    await send(first.socket, keyboard(2));
    await waitFor(
      () => fileContains(run.commandLog, `${keyboard(2).toString("hex")}\n`),
      "second key reaching the native peer before release",
    );
    await send(first.socket, "release");
    assert.equal((await nextJson(first, "control-state")).state, "ready");

    second = await openSocket(`${wsBase}/control`);
    await nextJson(second, "cursor");
    await send(second.socket, "acquire");
    assert.equal((await nextJson(second, "control-state")).state, "active");
    await send(second.socket, keyboard(3));
    await waitFor(
      () => fileContains(run.commandLog, `${keyboard(3).toString("hex")}\n`),
      "third key reaching the native peer before disconnect",
    );

    await delay(1_000);
    assert.equal(
      await fetchStatus(`${base}/healthz`),
      200,
      "an idle RTP stream made the desktop unhealthy",
    );

    await closeSocket(first);
    first = undefined;
    await closeSocket(second);
    second = undefined;
    await waitFor(
      async () =>
        (await readFile(run.commandLog, "utf8").catch(() => ""))
          .split("\n")
          .filter(Boolean).length >= 10,
      "ordered input handoff commands",
    );
    const records = (await readFile(run.commandLog, "utf8")).trim().split("\n");
    assert(records.includes(wire.keyframeReadyGeneration4));
    assert(records.includes(wire.releaseAll));
    const inputKinds = records
      .filter((record) => record !== wire.keyframeReadyGeneration4)
      .map((record) => Buffer.from(record, "hex")[1]);
    assert.deepEqual(inputKinds, [5, 4, 5, 5, 4, 5, 5, 4, 5]);

    await closeSocket(stream);
    stream = undefined;
    const outcome = await run.terminate();
    assert.deepEqual(outcome, { code: 0, signal: null }, run.diagnostics());
    await assertOwnedDescendantGone(run);
  } finally {
    if (first) await closeSocket(first).catch(() => {});
    if (second) await closeSocket(second).catch(() => {});
    if (stream) await closeSocket(stream).catch(() => {});
    await run.cleanup();
  }
}

async function checkConnectionAdmissionAndShutdown(
  gateway: string,
): Promise<void> {
  const run = await GatewayRun.start(gateway, "live", {
    env: {
      SPRITE_GATEWAY_FIXTURE_METADATA_DELAY_MS: "100",
      SPRITE_GATEWAY_FIXTURE_RTP_DELAY_MS: "1500",
    },
  });
  const base = `http://127.0.0.1:${run.port}`;
  const wsBase = `ws://127.0.0.1:${run.port}`;
  let stream: SocketInbox | undefined;
  const controls: SocketInbox[] = [];
  try {
    await waitFor(
      async () => (await fetchStatus(`${base}/healthz`)) === 503,
      "admission fixture HTTP server",
    );
    stream = await openSocket(`${wsBase}/stream`);
    await nextJson(stream, "video-config");
    for (let index = 1; index < GATEWAY_CONNECTION_LIMIT; index += 1) {
      controls.push(await openSocket(`${wsBase}/control`));
    }

    await rejectSocketStatus(`${wsBase}/control`, 503);
    const frame = await stream.next(
      "existing viewer frame at connection limit",
    );
    assert(
      frame.binary,
      "existing viewer stopped receiving at connection limit",
    );
    assert.equal(
      await fetchStatus(`${base}/healthz`),
      200,
      "connection rejection changed gateway health",
    );

    const released = controls.pop()!;
    await closeSocket(released);
    const replacement = await openSocket(`${wsBase}/control`);
    await nextJson(replacement, "cursor");
    controls.push(replacement);

    const closed = Promise.all(
      [stream.socket, ...controls.map((control) => control.socket)].map(
        waitForSocketClose,
      ),
    );
    const outcome = await run.terminate();
    assert.deepEqual(outcome, { code: 0, signal: null }, run.diagnostics());
    await bounded(closed, "gateway-owned WebSocket shutdown", 3_000);
    stream = undefined;
    controls.length = 0;
    await assertOwnedDescendantGone(run);
  } finally {
    if (stream) await closeSocket(stream).catch(() => {});
    await Promise.allSettled(controls.map(closeSocket));
    await run.cleanup();
  }
}

async function checkControlBoundsAndPipeLoad(gateway: string): Promise<void> {
  const run = await GatewayRun.start(gateway, "blocked-events");
  const base = `http://127.0.0.1:${run.port}`;
  const wsBase = `ws://127.0.0.1:${run.port}`;
  let probe: SocketInbox | undefined;
  let oversized: SocketInbox | undefined;
  let owner: SocketInbox | undefined;
  try {
    await waitFor(
      async () => (await fetchStatus(`${base}/healthz`)) === 200,
      "blocked-pipe fixture readiness",
    );

    probe = await openSocket(`${wsBase}/control`);
    await nextJson(probe, "cursor");
    const worstCase = JSON.stringify({
      type: "clipboard-write",
      text: "\u0001".repeat(1 << 20),
    });
    assert(worstCase.length > 6 * (1 << 20));
    await send(probe.socket, worstCase);
    await send(probe.socket, JSON.stringify({ type: "ping", id: 71 }));
    assert.equal((await nextJson(probe, "pong")).id, 71);

    oversized = await openSocket(`${wsBase}/control`);
    await nextJson(oversized, "cursor");
    await send(
      oversized.socket,
      JSON.stringify({
        type: "clipboard-write",
        text: "x".repeat((1 << 20) + 1),
      }),
    );
    await bounded(
      new Promise<void>((done) =>
        oversized!.socket.once("close", () => done()),
      ),
      "decoded clipboard limit rejection",
    );
    oversized = undefined;

    owner = await openSocket(`${wsBase}/control`);
    await nextJson(owner, "cursor");
    await send(owner.socket, "acquire");
    assert.equal((await nextJson(owner, "control-state")).state, "active");
    const large = JSON.stringify({
      type: "clipboard-write",
      text: "x".repeat(1_048_000),
    });
    await send(owner.socket, large);
    await send(owner.socket, JSON.stringify({ type: "ping", id: 72 }));
    assert.equal((await nextJson(owner, "pong")).id, 72);
    const peerPid = await readPid(run.peerPidFile);
    assert(peerPid, "owned blocked-input peer has no PID");
    process.kill(peerPid, "SIGUSR2");
    await nextJson(owner, "cursor"); // Fresh event while daemon stdin is blocked.
    owner.discardMessages();
    probe.discardMessages();
    for (let index = 0; index < 4; index += 1) owner.socket.send(large);
    await waitFor(
      () => fileContains(run.stateLog, "event-batch\n"),
      "simultaneous daemon event load",
    );
    const outcome = await run.waitForExit();
    assert.notEqual(
      outcome.code,
      0,
      "blocked daemon stdin ended as healthy shutdown",
    );
    assert.match(
      run.diagnostics(),
      /daemon command byte budget stalled|daemon stdin stalled/,
    );
    await assertOwnedDescendantGone(run);
  } finally {
    if (probe) await closeSocket(probe).catch(() => {});
    if (oversized) await closeSocket(oversized).catch(() => {});
    if (owner) await closeSocket(owner).catch(() => {});
    await run.cleanup();
  }
}

async function checkStalledControlOutput(gateway: string): Promise<void> {
  const run = await GatewayRun.start(gateway, "stalled-output");
  const base = `http://127.0.0.1:${run.port}`;
  const wsBase = `ws://127.0.0.1:${run.port}`;
  let owner: SocketInbox | undefined;
  let replacement: SocketInbox | undefined;
  try {
    await waitFor(
      async () => (await fetchStatus(`${base}/healthz`)) === 200,
      "stalled-output fixture readiness",
    );
    owner = await openSocket(`${wsBase}/control`);
    await nextJson(owner, "cursor");
    await send(owner.socket, "acquire");
    assert.equal((await nextJson(owner, "control-state")).state, "active");

    owner.discardMessages();
    owner.socket.pause();
    const peerPid = await readPid(run.peerPidFile);
    assert(peerPid, "stalled-output peer has no PID");
    process.kill(peerPid, "SIGUSR2");
    await waitFor(
      () => fileContains(run.stateLog, "stalled-ready\n"),
      "native clipboard burst entering the stalled control writer",
    );

    for (let sequence = 100; sequence < 112; sequence += 1) {
      await send(owner.socket, keyboard(sequence));
    }
    await waitFor(
      () => fileContains(run.commandLog, `${keyboard(111).toString("hex")}\n`),
      "numbered keys crossing a stalled control output",
      4_000,
    );

    const closed = waitForSocketClose(owner.socket);
    process.kill(peerPid, "SIGUSR1");
    await waitFor(
      () => fileContains(run.stateLog, "overflow-ready\n"),
      "native output overflow burst",
    );
    owner.socket.resume();
    await bounded(closed, "bounded slow-client disconnect", 5_000);
    owner = undefined;

    replacement = await openSocket(`${wsBase}/control`);
    await nextJson(replacement, "cursor");
    await send(replacement.socket, "acquire");
    assert.equal(
      (await nextJson(replacement, "control-state")).state,
      "active",
      "disconnect did not release the input lease",
    );
    await send(replacement.socket, keyboard(112));
    await waitFor(
      () => fileContains(run.commandLog, `${keyboard(112).toString("hex")}\n`),
      "replacement owner input",
    );

    await closeSocket(replacement);
    replacement = undefined;
    const outcome = await run.terminate();
    assert.deepEqual(outcome, { code: 0, signal: null }, run.diagnostics());
    await assertOwnedDescendantGone(run);
  } finally {
    if (owner) {
      owner.socket.resume();
      await closeSocket(owner).catch(() => {});
    }
    if (replacement) await closeSocket(replacement).catch(() => {});
    await run.cleanup();
  }
}

async function checkEventFailures(gateway: string): Promise<void> {
  const cases = [
    ["partial-event", /Truncated|halfway|event reader/i],
    ["truncated-event", /Truncated|halfway|event reader/i],
    ["malformed-event", /invalid event header|event reader/i],
    ["oversized-event", /byte limit|event reader/i],
  ] as const;
  for (const [mode, diagnostic] of cases) {
    const run = await GatewayRun.start(gateway, mode);
    try {
      const outcome = await run.waitForExit(6_000);
      assert.notEqual(outcome.code, 0, `${mode} ended as a healthy shutdown`);
      assert.match(
        run.diagnostics(),
        diagnostic,
        `${mode}: ${run.diagnostics()}`,
      );
      await assertOwnedDescendantGone(run);
    } finally {
      await run.cleanup();
    }
  }
}

async function checkLeaderExitCleanup(gateway: string): Promise<void> {
  const run = await GatewayRun.start(gateway, "leader-exit");
  try {
    const outcome = await run.waitForExit();
    assert.notEqual(outcome.code, 0, "peer leader exit looked healthy");
    assert.match(
      run.diagnostics(),
      /daemon (event pipe closed|exited)|event reader/i,
    );
    await assertOwnedDescendantGone(run);
  } finally {
    await run.cleanup();
  }
}

async function checkBindFailureDoesNotStartPeer(
  gateway: string,
): Promise<void> {
  const port = await freePort();
  const blocker = createServer();
  await bounded(
    new Promise<void>((done, fail) =>
      blocker.once("error", fail).listen(port, "127.0.0.1", done),
    ),
    "collision listener",
  );
  const run = await GatewayRun.start(gateway, "live", { port });
  try {
    const outcome = await run.waitForExit();
    assert.notEqual(outcome.code, 0, "HTTP bind collision exited successfully");
    assert.match(run.diagnostics(), /bind HTTP listener/);
    assert.equal(
      await readPid(run.peerPidFile),
      undefined,
      "peer started before HTTP bind",
    );
    assert.equal(
      await readPid(run.descendantPidFile),
      undefined,
      "peer descendant started before HTTP bind",
    );
  } finally {
    await run.cleanup();
    await closeServer(blocker);
  }
}

async function main(): Promise<void> {
  const gateway = gatewayArgument();
  await access(gateway).catch(() => {
    throw new Error(
      `gateway binary does not exist at ${gateway}; run cargo build -p sprite-desktop-gateway first`,
    );
  });
  await access(peer);
  const wire = JSON.parse(
    await readFile(join(fixtureDirectory, "wire-v2.json"), "utf8"),
  ) as GoldenWire;
  assert.match(wire.source, /90564cfb02030c494939c6fdf29cae9c4d689c67/);

  try {
    for (const [name, check] of [
      ["bind collision", () => checkBindFailureDoesNotStartPeer(gateway)],
      ["live gateway", () => checkLiveGateway(gateway, wire)],
      [
        "connection admission",
        () => checkConnectionAdmissionAndShutdown(gateway),
      ],
      [
        "control bounds and pipe load",
        () => checkControlBoundsAndPipeLoad(gateway),
      ],
      ["stalled control output", () => checkStalledControlOutput(gateway)],
      ["event failures", () => checkEventFailures(gateway)],
      ["leader exit", () => checkLeaderExitCleanup(gateway)],
    ] as const) {
      try {
        await check();
      } catch (cause) {
        throw new Error(`Gateway IPC check failed: ${name}`, { cause });
      }
    }
    console.log("gateway IPC/HTTP fixture checks passed");
  } finally {
    for (const socket of activeSockets) socket.terminate();
    await Promise.allSettled([...activeRuns].map((run) => run.cleanup()));
  }
}

await main();
