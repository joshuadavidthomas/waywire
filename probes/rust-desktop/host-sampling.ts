import { execFile } from "node:child_process";
import { readdir, readFile } from "node:fs/promises";
import os from "node:os";
import { performance } from "node:perf_hooks";

const SAMPLE_INTERVAL_MS = 1_000;
const HEARTBEAT_INTERVAL_MS = 100;
const MAX_SAMPLES = 40;
const MAX_HEARTBEATS = 600;
const MAX_PROCESSES = 64;
const MAX_THREADS = 512;
const MAX_TOP_THREADS = 12;
const MAX_ERRORS = 128;
const COMMAND_TIMEOUT_MS = 2_000;

export type HostSamplingErrorCode =
  | "GETCONF_FAILED"
  | "GETCONF_INVALID"
  | "ROOT_UNAVAILABLE"
  | "ROOT_IDENTITY_CHANGED"
  | "PS_FAILED"
  | "PS_INVALID_OUTPUT"
  | "PROC_READ_FAILED"
  | "COLLECTION_FAILED"
  | "PROCESS_LIMIT"
  | "THREAD_LIMIT"
  | "SAMPLE_LIMIT"
  | "HEARTBEAT_LIMIT";

export interface HostSamplingError {
  readonly atPerformanceMs: number;
  readonly code: HostSamplingErrorCode;
}

export interface HostProcessSample {
  readonly pid: number;
  readonly ppid: number;
  readonly comm: string;
  readonly startTicks: string;
  readonly cpuPercentOneCore: number | null;
  readonly identityChanged: boolean;
}

export interface HostThreadSample {
  readonly pid: number;
  readonly tid: number;
  readonly comm: string;
  readonly startTicks: string;
  readonly cpuPercentOneCore: number | null;
  readonly identityChanged: boolean;
}

export interface HostSample {
  readonly atPerformanceMs: number;
  readonly intervalMs: number;
  readonly hostCpuPercentAllCores: number | null;
  readonly loadAverage: readonly [number, number, number];
  readonly nodeCpuPercentOneCore: number | null;
  readonly processes: readonly HostProcessSample[];
  readonly topThreads: readonly HostThreadSample[];
}

export interface HostSamplingSnapshot {
  readonly schema: 1;
  readonly driverPid: number;
  readonly rootStartTicks: string | null;
  readonly startPerformanceMs: number;
  readonly finishPerformanceMs: number;
  readonly clockTicksPerSecond: number | null;
  readonly samples: readonly HostSample[];
  readonly heartbeat: readonly {
    readonly atPerformanceMs: number;
    readonly delayMs: number;
  }[];
  readonly errors: readonly HostSamplingError[];
  readonly overflow: {
    readonly samples: number;
    readonly heartbeat: number;
    readonly processes: number;
    readonly threads: number;
    readonly errors: number;
  };
}

interface ProcStat {
  readonly pid: number;
  readonly comm: string;
  readonly ppid: number;
  readonly cpuTicks: bigint;
  readonly startTicks: bigint;
  readonly rssPages: bigint;
}

interface ProcessTreeRow {
  readonly pid: number;
  readonly ppid: number;
}

interface Reading {
  readonly pid: number;
  readonly ppid: number;
  readonly stat: ProcStat;
  readonly threads: readonly ProcStat[];
}

interface State {
  readonly atPerformanceMs: number;
  readonly hostTicks: { readonly total: number; readonly busy: number };
  readonly nodeCpuMicros: number;
  readonly readings: readonly Reading[];
}

function parseProcStat(text: string): ProcStat {
  const match = /^(\d+) \((.*)\) (\S) (.+)\n?$/u.exec(text);
  if (!match) throw new Error("invalid proc stat");
  const tail = match[4]!.trim().split(/\s+/u);
  if (tail.length < 21 || !tail.every((field) => /^-?\d+$/u.test(field)))
    throw new Error("invalid proc stat fields");
  const pid = Number(match[1]);
  const ppid = Number(tail[0]);
  if (!Number.isSafeInteger(pid) || pid < 1 || !Number.isSafeInteger(ppid))
    throw new Error("invalid proc stat identity");
  return {
    pid,
    comm: match[2]!,
    ppid,
    cpuTicks: BigInt(tail[10]!) + BigInt(tail[11]!),
    startTicks: BigInt(tail[18]!),
    rssPages: BigInt(tail[20]!),
  };
}

function parseProcessTree(text: string): ProcessTreeRow[] {
  if (text.length > 4 * 1024 * 1024) throw new Error("ps output too large");
  const rows: ProcessTreeRow[] = [];
  const seen = new Set<number>();
  for (const line of text.split("\n")) {
    if (!line.trim()) continue;
    const match = /^\s*(\d+)\s+(\d+)\s*$/u.exec(line);
    if (!match) throw new Error("non-numeric ps output");
    const pid = Number(match[1]);
    const ppid = Number(match[2]);
    if (
      !Number.isSafeInteger(pid) ||
      pid < 1 ||
      !Number.isSafeInteger(ppid) ||
      seen.has(pid)
    )
      throw new Error("invalid ps identity");
    seen.add(pid);
    rows.push({ pid, ppid });
  }
  return rows;
}

function cpuPercent(
  currentTicks: bigint,
  previousTicks: bigint,
  elapsedMs: number,
  clockTicksPerSecond: number,
): number | null {
  const delta = currentTicks - previousTicks;
  if (delta < 0n || elapsedMs <= 0) return null;
  return (Number(delta) * 100_000) / clockTicksPerSecond / elapsedMs;
}

function statCpuPercent(
  current: ProcStat,
  previous: ProcStat | undefined,
  elapsedMs: number,
  clockTicksPerSecond: number,
): number | null {
  if (!previous || current.startTicks !== previous.startTicks) return null;
  return cpuPercent(
    current.cpuTicks,
    previous.cpuTicks,
    elapsedMs,
    clockTicksPerSecond,
  );
}

function hostTicks(): { total: number; busy: number } {
  let total = 0;
  let busy = 0;
  for (const cpu of os.cpus()) {
    const values = Object.values(cpu.times);
    total += values.reduce((sum, value) => sum + value, 0);
    busy += cpu.times.user + cpu.times.nice + cpu.times.sys + cpu.times.irq;
  }
  return { total, busy };
}

function runNumericCommand(
  file: string,
  args: readonly string[],
): Promise<string> {
  return new Promise((resolve, reject) => {
    execFile(
      file,
      [...args],
      {
        encoding: "utf8",
        timeout: COMMAND_TIMEOUT_MS,
        maxBuffer: 4 * 1024 * 1024,
        windowsHide: true,
      },
      (error, stdout) => (error ? reject(error) : resolve(stdout)),
    );
  });
}

function descendants(
  rows: readonly ProcessTreeRow[],
  rootPid: number,
): ProcessTreeRow[] {
  const byParent = new Map<number, ProcessTreeRow[]>();
  for (const row of rows) {
    const children = byParent.get(row.ppid) ?? [];
    children.push(row);
    byParent.set(row.ppid, children);
  }
  const root = rows.find(({ pid }) => pid === rootPid);
  if (!root) return [];
  const owned: ProcessTreeRow[] = [root];
  const queued = [rootPid];
  const visited = new Set(queued);
  for (let index = 0; index < queued.length; index += 1) {
    for (const child of byParent.get(queued[index]!) ?? []) {
      if (visited.has(child.pid)) continue;
      visited.add(child.pid);
      owned.push(child);
      queued.push(child.pid);
    }
  }
  return owned;
}

export async function startHostSampling(driverPid: number): Promise<{
  finish(): Promise<HostSamplingSnapshot>;
}> {
  if (!Number.isSafeInteger(driverPid) || driverPid < 1)
    throw new TypeError("driverPid must be a positive integer");

  const startPerformanceMs = performance.now();
  const samples: HostSample[] = [];
  const heartbeat: { atPerformanceMs: number; delayMs: number }[] = [];
  const errors: HostSamplingError[] = [];
  const overflow = {
    samples: 0,
    heartbeat: 0,
    processes: 0,
    threads: 0,
    errors: 0,
  };
  let cancelled = false;
  let sampleTimer: NodeJS.Timeout | undefined;
  let heartbeatTimer: NodeJS.Timeout | undefined;
  let inFlight: Promise<void> | undefined;
  let finished: Promise<HostSamplingSnapshot> | undefined;
  let sampleAttempts = 0;
  let clockTicksPerSecond: number | null = null;
  let rootStartTicks: bigint | null = null;
  let previous: State | undefined;

  const recordError = (code: HostSamplingErrorCode) => {
    if (errors.length < MAX_ERRORS)
      errors.push({ atPerformanceMs: performance.now(), code });
    else overflow.errors += 1;
  };

  try {
    const value = (await runNumericCommand("getconf", ["CLK_TCK"])).trim();
    if (!/^\d+$/u.test(value)) recordError("GETCONF_INVALID");
    else {
      const parsed = Number(value);
      if (Number.isSafeInteger(parsed) && parsed > 0)
        clockTicksPerSecond = parsed;
      else recordError("GETCONF_INVALID");
    }
  } catch {
    recordError("GETCONF_FAILED");
  }

  try {
    rootStartTicks = parseProcStat(
      await readFile(`/proc/${driverPid}/stat`, "utf8"),
    ).startTicks;
  } catch {
    recordError("ROOT_UNAVAILABLE");
  }

  const collectState = async (): Promise<State | undefined> => {
    if (rootStartTicks === null || clockTicksPerSecond === null)
      return undefined;
    let root: ProcStat;
    try {
      root = parseProcStat(await readFile(`/proc/${driverPid}/stat`, "utf8"));
    } catch {
      recordError("ROOT_UNAVAILABLE");
      cancelled = true;
      return undefined;
    }
    if (root.startTicks !== rootStartTicks) {
      recordError("ROOT_IDENTITY_CHANGED");
      cancelled = true;
      return undefined;
    }

    let output: string;
    try {
      output = await runNumericCommand("ps", ["-eo", "pid=,ppid="]);
    } catch {
      recordError("PS_FAILED");
      return undefined;
    }
    let rows: ProcessTreeRow[];
    try {
      rows = parseProcessTree(output);
    } catch {
      recordError("PS_INVALID_OUTPUT");
      return undefined;
    }
    const allOwned = descendants(rows, driverPid);
    if (allOwned.length === 0) {
      recordError("ROOT_UNAVAILABLE");
      cancelled = true;
      return undefined;
    }
    if (allOwned.length > MAX_PROCESSES) {
      overflow.processes += allOwned.length - MAX_PROCESSES;
      recordError("PROCESS_LIMIT");
    }
    const owned = allOwned.slice(0, MAX_PROCESSES);
    const readings: Reading[] = [];
    let threadCount = 0;
    let threadLimitRecorded = false;
    for (const row of owned) {
      try {
        const stat =
          row.pid === driverPid
            ? root
            : parseProcStat(await readFile(`/proc/${row.pid}/stat`, "utf8"));
        if (stat.pid !== row.pid || stat.ppid !== row.ppid) {
          recordError("PROC_READ_FAILED");
          continue;
        }
        const tids = (await readdir(`/proc/${row.pid}/task`))
          .filter((name) => /^\d+$/u.test(name))
          .map(Number)
          .toSorted((left, right) => left - right);
        const threads: ProcStat[] = [];
        for (const tid of tids) {
          if (threadCount >= MAX_THREADS) {
            overflow.threads += 1;
            if (!threadLimitRecorded) {
              recordError("THREAD_LIMIT");
              threadLimitRecorded = true;
            }
            continue;
          }
          threadCount += 1;
          try {
            threads.push(
              parseProcStat(
                await readFile(`/proc/${row.pid}/task/${tid}/stat`, "utf8"),
              ),
            );
          } catch {
            recordError("PROC_READ_FAILED");
          }
        }
        readings.push({ pid: row.pid, ppid: row.ppid, stat, threads });
      } catch {
        recordError("PROC_READ_FAILED");
      }
    }
    const usage = process.cpuUsage();
    return {
      atPerformanceMs: performance.now(),
      hostTicks: hostTicks(),
      nodeCpuMicros: usage.user + usage.system,
      readings,
    };
  };

  const collect = async () => {
    const current = await collectState();
    if (!current) return;
    if (previous) {
      if (samples.length >= MAX_SAMPLES) {
        overflow.samples += 1;
        recordError("SAMPLE_LIMIT");
        cancelled = true;
      } else {
        const elapsedMs = current.atPerformanceMs - previous.atPerformanceMs;
        const previousProcesses = new Map(
          previous.readings.map((item) => [item.pid, item]),
        );
        const previousThreads = new Map(
          previous.readings.flatMap((item) =>
            item.threads.map(
              (thread) => [`${item.pid}:${thread.pid}`, thread] as const,
            ),
          ),
        );
        const processes = current.readings.map((item): HostProcessSample => {
          const before = previousProcesses.get(item.pid);
          const same = before?.stat.startTicks === item.stat.startTicks;
          return {
            pid: item.pid,
            ppid: item.ppid,
            comm: item.stat.comm,
            startTicks: String(item.stat.startTicks),
            cpuPercentOneCore: statCpuPercent(
              item.stat,
              before?.stat,
              elapsedMs,
              clockTicksPerSecond!,
            ),
            identityChanged: before !== undefined && !same,
          };
        });
        const threads = current.readings.flatMap((item) =>
          item.threads.map((thread): HostThreadSample => {
            const before = previousThreads.get(`${item.pid}:${thread.pid}`);
            const same = before?.startTicks === thread.startTicks;
            return {
              pid: item.pid,
              tid: thread.pid,
              comm: thread.comm,
              startTicks: String(thread.startTicks),
              cpuPercentOneCore: statCpuPercent(
                thread,
                before,
                elapsedMs,
                clockTicksPerSecond!,
              ),
              identityChanged: before !== undefined && !same,
            };
          }),
        );
        const totalDelta = current.hostTicks.total - previous.hostTicks.total;
        const busyDelta = current.hostTicks.busy - previous.hostTicks.busy;
        samples.push({
          atPerformanceMs: current.atPerformanceMs,
          intervalMs: elapsedMs,
          hostCpuPercentAllCores:
            totalDelta > 0 ? (busyDelta * 100) / totalDelta : null,
          loadAverage: os.loadavg() as [number, number, number],
          nodeCpuPercentOneCore:
            elapsedMs > 0
              ? ((current.nodeCpuMicros - previous.nodeCpuMicros) * 100) /
                (elapsedMs * 1_000)
              : null,
          processes,
          topThreads: threads
            .toSorted(
              (left, right) =>
                (right.cpuPercentOneCore ?? -1) -
                (left.cpuPercentOneCore ?? -1),
            )
            .slice(0, MAX_TOP_THREADS),
        });
      }
    }
    previous = current;
  };

  const collectSafely = async () => {
    try {
      await collect();
    } catch {
      recordError("COLLECTION_FAILED");
    }
  };

  if (rootStartTicks !== null && clockTicksPerSecond !== null) {
    await collectSafely();
    const scheduleSample = () => {
      if (cancelled) return;
      sampleTimer = setTimeout(() => {
        sampleAttempts += 1;
        inFlight = collectSafely().finally(() => {
          inFlight = undefined;
          if (sampleAttempts >= MAX_SAMPLES) {
            overflow.samples += 1;
            recordError("SAMPLE_LIMIT");
            cancelled = true;
          }
          scheduleSample();
        });
      }, SAMPLE_INTERVAL_MS);
      sampleTimer.unref();
    };
    scheduleSample();
  }

  let expectedHeartbeat = performance.now() + HEARTBEAT_INTERVAL_MS;
  const beat = () => {
    if (cancelled) return;
    const now = performance.now();
    const delayMs = Math.max(0, now - expectedHeartbeat);
    const delayMissed = Math.floor(delayMs / HEARTBEAT_INTERVAL_MS);
    expectedHeartbeat += (delayMissed + 1) * HEARTBEAT_INTERVAL_MS;
    if (heartbeat.length < MAX_HEARTBEATS)
      heartbeat.push({ atPerformanceMs: now, delayMs });
    else {
      overflow.heartbeat += 1;
      recordError("HEARTBEAT_LIMIT");
      return;
    }
    heartbeatTimer = setTimeout(
      beat,
      Math.max(0, expectedHeartbeat - performance.now()),
    );
    heartbeatTimer.unref();
  };
  heartbeatTimer = setTimeout(beat, HEARTBEAT_INTERVAL_MS);
  heartbeatTimer.unref();

  return {
    finish() {
      if (finished) return finished;
      finished = (async () => {
        cancelled = true;
        if (sampleTimer) clearTimeout(sampleTimer);
        if (heartbeatTimer) clearTimeout(heartbeatTimer);
        await inFlight;
        return {
          schema: 1,
          driverPid,
          rootStartTicks:
            rootStartTicks === null ? null : String(rootStartTicks),
          startPerformanceMs,
          finishPerformanceMs: performance.now(),
          clockTicksPerSecond,
          samples,
          heartbeat,
          errors,
          overflow,
        };
      })();
      return finished;
    },
  };
}

export const hostSamplingTesting = {
  cpuPercent,
  descendants,
  parseProcessTree,
  parseProcStat,
  statCpuPercent,
};
