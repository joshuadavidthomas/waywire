import assert from "node:assert/strict";
import { chmod, mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { networkInterfaces, tmpdir } from "node:os";
import { delimiter, join } from "node:path";
import { fileURLToPath } from "node:url";
import { connect, createServer, type Server, type Socket } from "node:net";
import { once } from "node:events";
import { after, before, describe, it } from "node:test";
import { setTimeout as sleep } from "node:timers/promises";
import { startSpriteTunnel } from "./sprite-tunnel.js";
import { startTcpSampling } from "./tcp-sampling.js";

const fixturePath = fileURLToPath(
  new URL("fixtures/tunnel-peer.ts", import.meta.url),
);
const originalPath = process.env.PATH;
let commandDirectory: string;

function clearFixtureEnvironment() {
  delete process.env.TUNNEL_FIXTURE_MODE;
  delete process.env.TUNNEL_FIXTURE_PEER_ADDRESS;
  delete process.env.TUNNEL_FIXTURE_PEER_PORT;
  delete process.env.TUNNEL_FIXTURE_PEER_COUNT;
  delete process.env.TUNNEL_FIXTURE_PID_FILE;
}

async function closeServer(server: Server) {
  if (!server.listening) return;
  await new Promise<void>((resolve, reject) =>
    server.close((error) => (error ? reject(error) : resolve())),
  );
}

async function waitForProcessExit(pid: number) {
  const deadline = Date.now() + 3_000;
  while (Date.now() < deadline) {
    try {
      process.kill(pid, 0);
      await sleep(25);
    } catch (error) {
      if ((error as NodeJS.ErrnoException).code === "ESRCH") return;
      throw error;
    }
  }
  assert.fail(`process ${pid} remained alive`);
}

function nonLoopbackIpv4(): string | undefined {
  for (const addresses of Object.values(networkInterfaces())) {
    for (const address of addresses ?? []) {
      if (address.family === "IPv4" && !address.internal)
        return address.address;
    }
  }
  return undefined;
}

before(async () => {
  commandDirectory = await mkdtemp(join(tmpdir(), "sprite-tunnel-test-"));
  const commandPath = join(commandDirectory, "sprite");
  await writeFile(
    commandPath,
    `#!/bin/sh\nexec "${process.execPath}" --experimental-strip-types "${fixturePath}" "$@"\n`,
    { mode: 0o700 },
  );
  await chmod(commandPath, 0o700);
  process.env.PATH = `${commandDirectory}${delimiter}${originalPath ?? ""}`;
});

after(async () => {
  clearFixtureEnvironment();
  if (originalPath === undefined) delete process.env.PATH;
  else process.env.PATH = originalPath;
  await rm(commandDirectory, { recursive: true, force: true });
});

describe("startSpriteTunnel", { concurrency: false }, () => {
  it("owns a loopback tunnel and reports only its remote source ports", async (context) => {
    const address = nonLoopbackIpv4();
    if (!address) {
      context.skip("no non-loopback IPv4 address is available");
      return;
    }
    clearFixtureEnvironment();
    const accepted = new Set<Socket>();
    const peer = createServer((socket) => {
      accepted.add(socket);
      socket.once("close", () => accepted.delete(socket));
    });
    peer.listen(0, address);
    await once(peer, "listening");
    const peerAddress = peer.address();
    assert(peerAddress && typeof peerAddress !== "string");
    process.env.TUNNEL_FIXTURE_PEER_ADDRESS = address;
    process.env.TUNNEL_FIXTURE_PEER_PORT = String(peerAddress.port);
    process.env.TUNNEL_FIXTURE_PEER_COUNT = "2";

    const tunnel = await startSpriteTunnel("sprite-desktop-rust");
    let localClient: Socket | undefined;
    try {
      assert.equal(tunnel.port, 3218);
      assert(Number.isInteger(tunnel.pid) && tunnel.pid > 1);
      tunnel.assertAlive();

      localClient = connect({ host: "127.0.0.1", port: tunnel.port });
      await once(localClient, "connect");
      const first = await tunnel.remotePorts();
      const second = await tunnel.remotePorts();
      assert.equal(first.length, 2);
      assert.deepEqual(
        first,
        first.toSorted((left, right) => left - right),
      );
      assert.deepEqual(second, first);
      assert.equal(new Set(first).size, first.length);
      const tcp = startTcpSampling(first, tunnel);
      const snapshot = await tcp.finish();
      assert.equal(snapshot.ownerPid, tunnel.pid);
      assert.equal(snapshot.samples.length, 1);
      assert.equal(snapshot.samples[0]!.sockets.length, 2);
      assert.deepEqual(snapshot.errors, []);
      await tunnel.close();
      assert.throws(() => startTcpSampling(first, tunnel), /exited/u);
    } finally {
      localClient?.destroy();
      await tunnel.close();
      await tunnel.close();
      for (const socket of accepted) socket.destroy();
      await closeServer(peer);
      clearFixtureEnvironment();
    }
    assert.throws(() => tunnel.assertAlive(), /exited/u);
    await waitForProcessExit(tunnel.pid);
  });

  it("refuses a foreign listener without signalling its owner", async () => {
    clearFixtureEnvironment();
    const foreign = createServer();
    foreign.listen(3218, "127.0.0.1");
    await once(foreign, "listening");
    try {
      await assert.rejects(
        startSpriteTunnel("sprite-desktop-rust"),
        /already in use/u,
      );
      assert.equal(foreign.listening, true);
      const client = connect({ host: "127.0.0.1", port: 3218 });
      await once(client, "connect");
      client.destroy();
    } finally {
      await closeServer(foreign);
    }
  });

  it("reports a CLI that exits before readiness", async () => {
    clearFixtureEnvironment();
    process.env.TUNNEL_FIXTURE_MODE = "exit";
    try {
      await assert.rejects(
        startSpriteTunnel("sprite-desktop-rust"),
        /exited before the tunnel was ready/u,
      );
    } finally {
      clearFixtureEnvironment();
    }
  });

  it("kills and reaps its child when startup creates an unsafe listener", async () => {
    clearFixtureEnvironment();
    const directory = await mkdtemp(join(tmpdir(), "sprite-tunnel-pid-"));
    const pidPath = join(directory, "pid");
    process.env.TUNNEL_FIXTURE_MODE = "wrong-bind";
    process.env.TUNNEL_FIXTURE_PID_FILE = pidPath;
    try {
      await assert.rejects(
        startSpriteTunnel("sprite-desktop-rust"),
        /safe child-owned loopback listener/u,
      );
      const pid = Number(await readFile(pidPath, "utf8"));
      assert(Number.isInteger(pid) && pid > 1);
      await waitForProcessExit(pid);
    } finally {
      clearFixtureEnvironment();
      await rm(directory, { recursive: true, force: true });
    }
  });

  it("detects an owned child exit and keeps close idempotent", async () => {
    clearFixtureEnvironment();
    const tunnel = await startSpriteTunnel("sprite-desktop-rust");
    process.kill(tunnel.pid, "SIGTERM");
    await waitForProcessExit(tunnel.pid);
    assert.throws(() => tunnel.assertAlive(), /exited/u);
    await tunnel.close();
    await tunnel.close();
  });

  it("escalates to SIGKILL and reaps a child that ignores SIGTERM", async () => {
    clearFixtureEnvironment();
    process.env.TUNNEL_FIXTURE_MODE = "ignore-term";
    const tunnel = await startSpriteTunnel("sprite-desktop-rust");
    try {
      const firstClose = tunnel.close();
      const secondClose = tunnel.close();
      assert.strictEqual(secondClose, firstClose);
      await firstClose;
      await waitForProcessExit(tunnel.pid);
    } finally {
      clearFixtureEnvironment();
    }
  });

  it("checks the disposable target before launching a child", async () => {
    clearFixtureEnvironment();
    await assert.rejects(
      startSpriteTunnel("josh-desktop"),
      /protected Sprite/u,
    );
  });
});
