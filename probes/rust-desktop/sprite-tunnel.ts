import assert from "node:assert/strict";
import { execFile, spawn, type ChildProcess } from "node:child_process";
import { isIP } from "node:net";
import { setTimeout as sleep } from "node:timers/promises";
import { promisify } from "node:util";
import { assertDisposableTarget } from "./trial.js";

const exec = promisify(execFile);
const localPort = 3218;
const remotePort = 8080;
const readinessTimeoutMs = 15_000;
const terminationTimeoutMs = 2_000;
const socketOutputLimit = 1024 * 1024;

interface Endpoint {
  readonly address: string;
  readonly port: number;
}

export interface SpriteTunnel {
  readonly pid: number;
  readonly port: number;
  assertAlive(): void;
  remotePorts(): Promise<number[]>;
  close(): Promise<void>;
}

function parseEndpoint(value: string): Endpoint | undefined {
  const bracketed = /^\[([^\]]+)\]:(\d+)$/u.exec(value);
  const plain = /^(.*):(\d+)$/u.exec(value);
  const match = bracketed ?? plain;
  if (!match) return undefined;
  const address = (match[1] ?? "").split("%", 1)[0]!;
  const port = Number(match[2]);
  if (!Number.isInteger(port) || port < 1 || port > 65_535) return undefined;
  return { address, port };
}

function isLoopback(address: string): boolean {
  if (isIP(address) === 4) return address.startsWith("127.");
  const normalized = address.toLowerCase();
  if (isIP(address) !== 6) return false;
  if (normalized === "::1" || normalized === "0:0:0:0:0:0:0:1") return true;
  const mapped = /^::ffff:(\d+\.\d+\.\d+\.\d+)$/u.exec(normalized);
  return mapped ? mapped[1]!.startsWith("127.") : false;
}

function ownerPids(row: string): number[] {
  return [...row.matchAll(/(?:^|[,\s])pid=(\d+)(?=[,\s)])/gu)].map((match) =>
    Number(match[1]),
  );
}

async function listenerRows(): Promise<string[]> {
  try {
    const { stdout } = await exec(
      "ss",
      ["-Hlnpt", "sport", "=", `:${localPort}`],
      { timeout: 2_000, maxBuffer: 64 * 1024 },
    );
    return stdout.trim().split("\n").filter(Boolean);
  } catch {
    throw new Error("could not inspect the local tunnel port");
  }
}

function isOwnedLoopbackListener(row: string, pid: number): boolean {
  const fields = row.trim().split(/\s+/u);
  const endpoint =
    fields[0] === "LISTEN" ? parseEndpoint(fields[3] ?? "") : undefined;
  const owners = ownerPids(row);
  return (
    endpoint?.port === localPort &&
    isLoopback(endpoint.address) &&
    owners.length > 0 &&
    owners.every((owner) => owner === pid)
  );
}

function hasExited(child: ChildProcess): boolean {
  return child.exitCode !== null || child.signalCode !== null;
}

async function waitForClose(
  child: ChildProcess,
  alreadyClosed: () => boolean,
  timeoutMs: number,
): Promise<boolean> {
  if (alreadyClosed()) return true;
  return new Promise<boolean>((resolve) => {
    const finish = (closed: boolean) => {
      clearTimeout(timer);
      child.off("close", onClose);
      resolve(closed);
    };
    const onClose = () => finish(true);
    const timer = setTimeout(() => finish(false), timeoutMs);
    timer.unref();
    child.once("close", onClose);
    if (alreadyClosed()) finish(true);
  });
}

async function stopOwnedChild(
  child: ChildProcess,
  alreadyClosed: () => boolean,
): Promise<void> {
  if (alreadyClosed()) return;
  if (!hasExited(child)) child.kill("SIGTERM");
  if (await waitForClose(child, alreadyClosed, terminationTimeoutMs)) return;
  if (!hasExited(child)) child.kill("SIGKILL");
  if (!(await waitForClose(child, alreadyClosed, terminationTimeoutMs)))
    throw new Error("tunnel process did not exit after SIGKILL");
}

async function readRemotePorts(child: ChildProcess, pid: number) {
  assert(!hasExited(child), "Sprite tunnel has exited");
  let stdout: string;
  try {
    ({ stdout } = await exec("ss", ["-Htnp", "state", "established"], {
      timeout: 2_000,
      maxBuffer: socketOutputLimit,
    }));
  } catch {
    throw new Error("could not inspect tunnel TCP sockets");
  }
  assert(!hasExited(child), "Sprite tunnel exited while inspecting sockets");

  const ports = new Set<number>();
  for (const row of stdout.trim().split("\n").filter(Boolean)) {
    const owners = ownerPids(row);
    if (owners.length === 0 || !owners.every((owner) => owner === pid))
      continue;
    const fields = row.trim().split(/\s+/u);
    const local = parseEndpoint(fields[2] ?? "");
    const peer = parseEndpoint(fields[3] ?? "");
    if (!local || !peer || isLoopback(peer.address)) continue;
    ports.add(local.port);
  }
  const result = [...ports].sort((left, right) => left - right);
  assert(
    result.length >= 1 && result.length <= 4,
    "expected one to four child-owned remote TCP sockets",
  );
  return result;
}

export async function startSpriteTunnel(
  spriteName: string,
): Promise<SpriteTunnel> {
  assertDisposableTarget(spriteName);
  if ((await listenerRows()).length !== 0)
    throw new Error(`local port ${localPort} is already in use`);

  const child = spawn(
    "sprite",
    ["-s", spriteName, "proxy", `${localPort}:${remotePort}`],
    {
      stdio: "ignore",
    },
  );
  let closed = false;
  let spawnFailed = false;
  child.once("close", () => {
    closed = true;
  });
  const spawned = await new Promise<boolean>((resolve) => {
    child.once("spawn", () => resolve(true));
    child.once("error", () => {
      spawnFailed = true;
      resolve(false);
    });
  });
  if (!spawned || spawnFailed || child.pid === undefined) {
    await stopOwnedChild(child, () => closed);
    throw new Error("could not start the Sprite CLI");
  }
  const pid = child.pid;

  try {
    const deadline = Date.now() + readinessTimeoutMs;
    let ready = false;
    while (Date.now() < deadline) {
      const rows = await listenerRows();
      if (rows.length > 0) {
        if (
          rows.every((row) => isOwnedLoopbackListener(row, pid)) &&
          !hasExited(child)
        ) {
          ready = true;
          break;
        }
        throw new Error(
          `local port ${localPort} does not have a safe child-owned loopback listener`,
        );
      }
      if (hasExited(child))
        throw new Error("Sprite CLI exited before the tunnel was ready");
      await sleep(100);
    }
    if (!ready) throw new Error("Sprite tunnel did not become ready in time");
    const rows = await listenerRows();
    if (
      rows.length === 0 ||
      !rows.every((row) => isOwnedLoopbackListener(row, pid)) ||
      hasExited(child)
    )
      throw new Error("Sprite tunnel changed during readiness verification");
  } catch (error) {
    await stopOwnedChild(child, () => closed);
    throw error;
  }

  let closePromise: Promise<void> | undefined;
  let closing = false;
  const assertAlive = () => {
    assert(
      !closing && !closed && !hasExited(child),
      "Sprite tunnel has exited",
    );
  };
  return {
    pid,
    port: localPort,
    assertAlive,
    async remotePorts() {
      assertAlive();
      return readRemotePorts(child, pid);
    },
    close() {
      closing = true;
      closePromise ??= stopOwnedChild(child, () => closed);
      return closePromise;
    },
  };
}
