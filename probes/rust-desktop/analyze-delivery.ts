import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { resolve } from "node:path";
import { parseBrowserTimingTrace } from "./browser-timings.js";
import { parseGatewayTimingTrace } from "./gateway-timings.js";
import type { ProxyVideoTimingsSnapshot } from "./proxy-timings.js";
import type { HostSamplingSnapshot } from "./host-sampling.js";
import type { TcpSamplingSnapshot } from "./tcp-sampling.js";
import {
  heartbeatDelayWithinGap,
  summarizeTcpGap,
  summarizeDurations,
} from "./delivery-analysis.js";

const directory = process.argv[2];
assert(directory, "usage: analyze-delivery.ts RESULT_DIRECTORY");
const browser = parseBrowserTimingTrace(
  JSON.parse(
    await readFile(resolve(directory, "browser-timings.json"), "utf8"),
  ),
);
const gateway = parseGatewayTimingTrace(
  JSON.parse(
    await readFile(resolve(directory, "gateway-stage-timings.json"), "utf8"),
  ),
);
const delivery =
  process.argv[3] === "--proxy"
    ? (JSON.parse(
        await readFile(resolve(directory, "delivery.json"), "utf8"),
      ) as {
        schema: string;
        streams: ProxyVideoTimingsSnapshot[];
        observerOverflow: number;
        host: HostSamplingSnapshot;
        tcp?: TcpSamplingSnapshot;
        route?: "public" | "tunnel";
        tunnelTcp?: TcpSamplingSnapshot;
      })
    : undefined;
const proxyFrames = new Map<
  string,
  { first: number; complete: number; auBytes: number; captureNanos: string }
>();
if (delivery) {
  assert.equal(delivery.schema, "sprite-desktop-delivery-v1");
  assert.equal(delivery.observerOverflow, 0);
  for (const stream of delivery.streams) {
    assert.equal(stream.error, null);
    assert.equal(stream.overflow, 0);
    assert(stream.records.length <= 10_000);
    for (const record of stream.records) {
      const key = `${record.generation}:${record.sequence}`;
      assert(
        !proxyFrames.has(key),
        "analysis requires unique proxy frame identities",
      );
      assert(
        Number.isFinite(record.completedAtMs) &&
          record.completedAtMs >= record.firstByteAtMs,
      );
      proxyFrames.set(key, {
        first: stream.performanceStartMs + record.firstByteAtMs,
        complete: stream.performanceStartMs + record.completedAtMs,
        auBytes: record.auBytes,
        captureNanos: record.captureNanos,
      });
    }
  }
}
const writes = gateway.records.filter(
  (record) => record.stage === "socket_write_end",
);
const connections = new Set(writes.map((record) => record.connection_id));
assert.equal(
  connections.size,
  1,
  "analysis requires one traced video connection",
);
const sent = new Map(
  writes.map((record) => [
    `${record.generation}:${record.sequence}`,
    BigInt(record.timestamp_nanos),
  ]),
);
assert.equal(sent.size, writes.length, "duplicate socket completion for frame");
const gaps = browser.headers.slice(1).flatMap((current, index) => {
  const previous = browser.headers[index]!;
  if (current.generation !== previous.generation) return [];
  const previousSent = sent.get(`${previous.generation}:${previous.sequence}`);
  const currentSent = sent.get(`${current.generation}:${current.sequence}`);
  if (previousSent === undefined || currentSent === undefined) return [];
  const receiptGapMs = current.atMs - previous.atMs;
  const sendGapMs = Number(currentSent - previousSent) / 1e6;
  assert(sendGapMs >= 0, "gateway completions went backwards");
  const proxyPrevious = proxyFrames.get(
    `${previous.generation}:${previous.sequence}`,
  );
  const proxyCurrent = proxyFrames.get(
    `${current.generation}:${current.sequence}`,
  );
  const proxyGap =
    proxyCurrent && proxyPrevious
      ? proxyCurrent.complete - proxyPrevious.complete
      : undefined;
  if (proxyCurrent) {
    assert.equal(proxyCurrent.auBytes, current.auBytes);
    assert.equal(proxyCurrent.captureNanos, current.captureNanos);
  }
  return [
    {
      generation: current.generation,
      previousSequence: previous.sequence,
      sequence: current.sequence,
      receiptAtMs: current.atMs,
      receiptGapMs,
      sendGapMs,
      captureGapMs:
        Number(BigInt(current.captureNanos) - BigInt(previous.captureNanos)) /
        1e6,
      addedDeliveryGapMs: receiptGapMs - sendGapMs,
      ...(proxyGap !== undefined && proxyCurrent && proxyPrevious && delivery
        ? {
            proxyGapMs: proxyGap,
            addedGapBeforeProxyMs: proxyGap - sendGapMs,
            addedGapAfterProxyMs: receiptGapMs - proxyGap,
            proxyMessageSpanMs: proxyCurrent.complete - proxyCurrent.first,
            nodeHeartbeatDelayMs: heartbeatDelayWithinGap(
              delivery.host.heartbeat,
              proxyPrevious.complete,
              proxyCurrent.complete,
            ),
            tcp: delivery.tcp
              ? summarizeTcpGap(
                  delivery.tcp.samples,
                  proxyPrevious.complete,
                  proxyCurrent.complete,
                )
              : null,
            tunnelTcp: delivery.tunnelTcp
              ? summarizeTcpGap(
                  delivery.tunnelTcp.samples,
                  proxyPrevious.complete,
                  proxyCurrent.complete,
                )
              : null,
          }
        : {}),
      auBytes: current.auBytes,
      keyframe: Boolean(current.flags & 1),
      overlappingLongTasks: browser.longTasks.filter(
        (task) =>
          task.atMs < current.atMs &&
          task.atMs + task.durationMs > previous.atMs,
      ).length,
    },
  ];
});
console.log(
  JSON.stringify(
    {
      scope:
        "Each gap uses two timestamps on the same clock. Added delivery gap subtracts those durations, not clock epochs; clock-rate skew remains uncalibrated. Socket completion is local acceptance, so later delay includes kernel/proxies/network/browser delivery. No inference of one-way latency or physical presentation.",
      matchedIntervals: gaps.length,
      gapSummaryMs: {
        gatewayWrite: summarizeDurations(gaps.map((gap) => gap.sendGapMs)),
        browserReceipt: summarizeDurations(gaps.map((gap) => gap.receiptGapMs)),
        proxyReceipt: summarizeDurations(
          gaps.flatMap((gap) =>
            gap.proxyGapMs === undefined ? [] : [gap.proxyGapMs],
          ),
        ),
        addedBeforeProxy: summarizeDurations(
          gaps.flatMap((gap) =>
            gap.addedGapBeforeProxyMs === undefined
              ? []
              : [gap.addedGapBeforeProxyMs],
          ),
        ),
        addedAfterProxy: summarizeDurations(
          gaps.flatMap((gap) =>
            gap.addedGapAfterProxyMs === undefined
              ? []
              : [gap.addedGapAfterProxyMs],
          ),
        ),
      },
      ...(delivery
        ? {
            hostScope:
              "Approximate one-second CPU samples from the owned browser/driver tree; percent of one core. Host busy is across all cores. Times below use the Node probe clock, not the browser window clock.",
            tcpScope:
              "TCP samples bracket each gap with collection uncertainty; roughly 250 ms plus ss execution per sample. Absent counters mean unreported, not proof of no loss. Local retransmissions do not measure the remote video sender's retransmissions.",
            route: delivery.route ?? "not labelled in this recording",
            nodeSocketScope:
              delivery.route === "tunnel"
                ? "Node proxy to local CLI listener; these are loopback observations"
                : "Node proxy upstream socket",
            tunnelTcpScope: delivery.tunnelTcp
              ? "All remote TCP sockets owned by CLI at recording start, in stable source-port order. No assignment to video or control; no remote-sender retransmission measurement."
              : null,
            tunnelTcpSamples: delivery.tunnelTcp?.samples.length,
            tunnelTcpErrors: delivery.tunnelTcp?.errors,
            maximumHostBusyPercent: Math.max(
              0,
              ...delivery.host.samples.flatMap((sample) =>
                sample.hostCpuPercentAllCores === null
                  ? []
                  : [sample.hostCpuPercentAllCores],
              ),
            ),
            tcpSamples: delivery.tcp?.samples.length,
            tcpErrors: delivery.tcp?.errors,
            maximumSampledReceiveQueue: delivery.tcp
              ? Math.max(
                  0,
                  ...delivery.tcp.samples.flatMap((sample) =>
                    sample.sockets.map((socket) => socket.recvQueue),
                  ),
                )
              : null,
            hostErrors: delivery.host.errors,
            hostOverflow: delivery.host.overflow,
            hostTimeline: delivery.host.samples.map((sample) => ({
              atMs:
                sample.atPerformanceMs -
                delivery.streams[0]!.performanceStartMs,
              hostBusyPercent: sample.hostCpuPercentAllCores,
              probeCpuPercent: sample.nodeCpuPercentOneCore,
              ownedCpuPercent: sample.processes.reduce(
                (total, process) => total + (process.cpuPercentOneCore ?? 0),
                0,
              ),
              hottestThreads: sample.topThreads.slice(0, 3).map((thread) => ({
                name: thread.comm,
                cpu: thread.cpuPercentOneCore,
              })),
            })),
          }
        : {}),
      largestReceiptGaps: gaps
        .toSorted((a, b) => b.receiptGapMs - a.receiptGapMs)
        .slice(0, 10),
    },
    null,
    2,
  ),
);
