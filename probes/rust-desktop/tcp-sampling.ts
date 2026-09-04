import assert from "node:assert/strict";
import { execFile } from "node:child_process";
import { performance } from "node:perf_hooks";
import { promisify } from "node:util";

const exec = promisify(execFile);
const counterNames = [
  "bytes_sent",
  "bytes_acked",
  "bytes_received",
  "segs_out",
  "segs_in",
  "data_segs_out",
  "data_segs_in",
  "rcv_ooopack",
  "dsack_dups",
  "rcv_space",
  "rcv_ssthresh",
  "lastsnd",
  "lastrcv",
  "lastack",
  "rto",
  "ato",
  "unacked",
  "lost",
  "sacked",
  "retrans",
  "rtt",
  "rcv_rtt",
] as const;

// Only numeric queue/counter fields leave this parser. No addresses, process
// names, file descriptors, commands or packet content enter saved artifacts.
export function parseOwnedTcpInfo(text: string, ownerPid: number) {
  const lines = text.trim().split("\n");
  assert.equal(
    lines.length,
    2,
    "expected one established socket and its TCP info",
  );
  const row = lines[0]!.trim().split(/\s+/u);
  assert.equal(row[0], "ESTAB");
  const owners = [...lines[0]!.matchAll(/pid=(\d+)/gu)].map((match) =>
    Number(match[1]),
  );
  assert(
    owners.length > 0 && owners.every((pid) => pid === ownerPid),
    "socket ownership changed",
  );
  const recvQueue = Number(row[1]);
  const sendQueue = Number(row[2]);
  assert(Number.isSafeInteger(recvQueue) && recvQueue >= 0);
  assert(Number.isSafeInteger(sendQueue) && sendQueue >= 0);
  const counters: Partial<Record<(typeof counterNames)[number], number[]>> = {};
  for (const name of counterNames) {
    const match = new RegExp(
      `(?:^|\\s)${name}:([0-9.]+(?:/[0-9.]+)*)(?=\\s|$)`,
      "u",
    ).exec(lines[1]!);
    if (!match) continue;
    const values = match[1]!.split("/").map(Number);
    assert(values.every((value) => Number.isFinite(value) && value >= 0));
    counters[name] = values;
  }
  return { recvQueue, sendQueue, counters };
}

export type TcpSamplingSnapshot = Awaited<
  ReturnType<ReturnType<typeof startTcpSampling>["finish"]>
>;

export function startTcpSampling(
  ports: readonly number[],
  owner?: { readonly pid: number; assertAlive(): void },
) {
  const ownerPid = owner?.pid ?? process.pid;
  assert(Number.isSafeInteger(ownerPid) && ownerPid > 0);
  owner?.assertAlive();
  assert(ports.length > 0 && ports.length <= 4);
  assert(new Set(ports).size === ports.length);
  assert(
    ports.every((port) => Number.isInteger(port) && port > 0 && port <= 65535),
  );
  const samples: {
    atPerformanceMs: number;
    collectionMs: number;
    sockets: ReturnType<typeof parseOwnedTcpInfo>[];
  }[] = [];
  const errors: { atPerformanceMs: number; code: "TCP_SAMPLE_FAILED" }[] = [];
  let overflow = 0;
  let stopped = false;
  let timer: NodeJS.Timeout | undefined;
  let inFlight: Promise<void> | undefined;
  const poll = async () => {
    const started = performance.now();
    try {
      owner?.assertAlive();
      const sockets = [];
      for (const port of ports) {
        const { stdout } = await exec(
          "ss",
          ["-Htinp", "state", "established", `( sport = :${port} )`],
          { timeout: 2000, maxBuffer: 16 * 1024 },
        );
        // With a state filter ss omits the state column.
        owner?.assertAlive();
        sockets.push(parseOwnedTcpInfo(`ESTAB ${stdout}`, ownerPid));
      }
      samples.push({
        atPerformanceMs: performance.now(),
        collectionMs: performance.now() - started,
        sockets,
      });
    } catch {
      errors.push({
        atPerformanceMs: performance.now(),
        code: "TCP_SAMPLE_FAILED",
      });
    }
    if (stopped) return;
    if (samples.length + errors.length >= 140) {
      overflow += 1;
      return;
    }
    timer = setTimeout(run, 250);
    timer.unref();
  };
  const run = () => {
    inFlight = poll();
  };
  run();
  return {
    async finish() {
      stopped = true;
      if (timer) clearTimeout(timer);
      await inFlight;
      return { schema: 1, ownerPid, samples, errors, overflow };
    },
  };
}
