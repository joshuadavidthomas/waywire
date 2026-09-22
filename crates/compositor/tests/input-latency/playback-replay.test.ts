// Deterministic scheduling/clock replay, NOT a codec or physical-network benchmark.
import assert from "node:assert/strict";
import { writeFileSync } from "node:fs";
import { after, test } from "node:test";
import { ClockSynchronizer } from "../../../gateway/web/src/sdk/control.ts";
import { Playout } from "../../../gateway/web/src/sdk/playout.ts";
import { PROTOCOL_VERSION } from "../../../gateway/web/src/sdk/messages.ts";
import { VideoRuntime } from "../../../gateway/web/src/sdk/video.ts";
import {
  FakeTarget,
  FakeWebSocket,
  flush,
  installBrowser,
  installGlobal,
  installQueueVideoDecoder,
  socket,
  videoPacket,
} from "../../../gateway/web/src/sdk/test-support.ts";

const modes = ["newest", 0, 25, 50, 100, "adaptive"] as const;
const scenarios = ["stable", "jitter", "rtt-step", "asymmetric"] as const;
const serverOffsetMs = 1234;
const output: unknown[] = [];
const distribution = (values: number[]) => {
  const v = [...values].sort((a, b) => a - b);
  return {
    n: v.length,
    p50: v[Math.floor(v.length / 2)],
    p95: v[Math.ceil(v.length * 0.95) - 1],
    max: v.at(-1),
  };
};

for (const scenario of scenarios)
  for (const mode of modes) {
    test(`production replay ${scenario}/${mode}`, async (context) => {
      installBrowser();
      const installed = installQueueVideoDecoder();
      let now = 0;
      context.mock.method(performance, "now", () => now);
      context.mock.timers.enable({ apis: ["setTimeout"] });
      const callbacks = new Map<number, FrameRequestCallback>();
      let callbackId = 0;
      installGlobal(
        "requestAnimationFrame",
        (callback: FrameRequestCallback) => {
          callbacks.set(++callbackId, callback);
          return callbackId;
        },
      );
      installGlobal("cancelAnimationFrame", (id: number) =>
        callbacks.delete(id),
      );
      const clock = new ClockSynchronizer();
      const playout = new Playout();
      let target: number =
        typeof mode === "number" ? mode : mode === "newest" ? 0 : 100;
      let latestRtt = 0;
      const stream = new FakeWebSocket();
      const draws: Array<{
        atMs: number;
        captureMs: number;
        freshnessMs: number;
        latenessMs: number;
        targetMs: number;
        confident: boolean;
        decodeQueue: number;
        presentationQueue: number;
      }> = [];
      const confidence: Array<{ atMs: number; confident: boolean }> = [];
      const samples: Array<{
        sentMs: number;
        receivedMs: number;
        upMs: number;
        downMs: number;
        accepted: boolean;
        confident: boolean;
        offsetErrorMs: number;
      }> = [];
      const updateTarget = (
        sample: { latenessMs: number; decodeQueue: number } | null,
      ) => {
        if (mode !== "adaptive") return;
        target = playout.update(sample, now);
        video.setLatencyTarget(target);
      };
      const video = new VideoRuntime(
        {
          createSocket: async () => socket(stream),
          connectionGeneration: () => 1,
          shouldRun: () => true,
          disposed: () => false,
          setStatus() {},
          updateState() {},
          setLatestAppliedInput() {},
          presentResizeGeneration() {},
          emitError(error) {
            throw error;
          },
          halt(error) {
            throw error;
          },
          expectedPresentationTime: (timestamp, latency) =>
            mode === "newest"
              ? null
              : clock.expectedPresentationTime(timestamp, latency),
          statsContext: () => ({
            bitrateKbps: 16000,
            scalePercent: 100,
            rttMs: latestRtt,
            clockConfident: mode !== "newest" && clock.synchronized(),
            clockUncertaintyMs: Number.isFinite(clock.bestRttMilliseconds)
              ? clock.bestRttMilliseconds / 2
              : null,
            pendingInputCount: 0,
            resizeState: "idle",
          }),
          publishStats(stats) {
            const captureMs =
              stats.renderedMediaTimestampMicros! / 1000 - serverOffsetMs;
            draws.push({
              atMs: now,
              captureMs,
              freshnessMs: now - captureMs,
              latenessMs: stats.latenessMs,
              targetMs: stats.latencyTargetMs,
              confident: clock.synchronized(),
              decodeQueue: stats.decoderQueue,
              presentationQueue: stats.pendingVideoFrames,
            });
            updateTarget(
              stats.clockConfident
                ? {
                    latenessMs: stats.latenessMs,
                    decodeQueue: stats.decoderQueue,
                  }
                : null,
            );
          },
        },
        target,
        0,
      );
      video.attach(
        new FakeTarget() as unknown as HTMLCanvasElement,
        { drawImage() {} } as unknown as CanvasRenderingContext2D,
      );
      await video.connect();
      stream.dispatch("message", {
        data: JSON.stringify({
          type: "video-config",
          version: PROTOCOL_VERSION,
          codec: "avc1.F40034",
        }),
      });
      await flush();
      const duration = scenario === "rtt-step" ? 40000 : 20000;
      // Known clock ground truth is available ONLY in this synthetic replay.
      const path = (time: number, index: number): [number, number] => {
        if (scenario === "rtt-step" && time >= 5000) return [70, 70];
        if (scenario === "asymmetric") return [10, 50];
        if (scenario === "jitter")
          return [30, 30 + [0, 0, 15, 50, 0, 90, 5][index % 7]!];
        return [30, 30];
      };
      const pings = Array.from({ length: duration / 1000 }, (_, index) => {
        const sent = index * 1000,
          [up, down] = path(sent, index);
        return { sent, up, down, received: sent + up + down };
      });
      // TCP order is retained: a jittered packet holds subsequent arrivals.
      let lastArrival = -1;
      const arrivals = Array.from(
        { length: Math.ceil((duration * 60) / 1000) },
        (_, index) => {
          const capture = Math.round((index * 1000) / 60);
          const [, down] = path(capture, index);
          const arrival = Math.max(capture + 8 + down, lastArrival + 1);
          lastArrival = arrival;
          return {
            capture,
            arrival,
            timestamp: (capture + serverOffsetMs) * 1000,
          };
        },
      );
      const decodeDue = new Set<number>();
      let pingIndex = 0,
        frameIndex = 0,
        nextPaint = 0;
      const received: typeof arrivals = [];
      try {
        for (now = 0; now <= duration; now++) {
          if (now) context.mock.timers.tick(1);
          const ping = pings[pingIndex];
          if (ping?.received === now) {
            const before = Reflect.get(clock, "lastSampleAt");
            clock.update(
              ping.sent,
              now,
              (ping.sent + ping.up + serverOffsetMs) * 1e6,
            );
            latestRtt = ping.up + ping.down;
            samples.push({
              sentMs: ping.sent,
              receivedMs: now,
              upMs: ping.up,
              downMs: ping.down,
              accepted: Reflect.get(clock, "lastSampleAt") !== before,
              confident: clock.synchronized(),
              offsetErrorMs: (clock.offsetMicros ?? 0) / 1000 + serverOffsetMs,
            });
            pingIndex++;
          }
          const synchronized = clock.synchronized();
          if (confidence.at(-1)?.confident !== synchronized)
            confidence.push({ atMs: now, confident: synchronized });
          while (arrivals[frameIndex]?.arrival === now) {
            const frame = arrivals[frameIndex++]!;
            received.push(frame);
            stream.dispatch("message", {
              data: videoPacket(frame.timestamp, {
                keyframe: received.length % 15 === 1,
                width: 1920,
                height: 1080,
                fps: 60,
                chroma: 2,
              }),
            });
            await flush();
            decodeDue.add(now + 4);
          }
          // Simulated decoder drains its current batch at a scheduled completion.
          if (decodeDue.delete(now)) installed.decoder()?.outputAll();
          if (now >= nextPaint) {
            const ready = [...callbacks.values()];
            callbacks.clear();
            for (const callback of ready) callback(now);
            nextPaint += 1000 / 60;
          }
          if (now % 1000 === 0) updateTarget(null);
        }
        const steady = draws.filter(
          (d) => d.atMs >= 2000 && d.atMs <= duration - 1000,
        );
        assert.ok(steady.length > 0);
        assert.ok(
          draws.slice(1).every((d, i) => d.captureMs > draws[i]!.captureMs),
        );
        if (scenario === "stable")
          assert.ok(steady.length >= ((duration - 3000) / 1000) * 50);
        assert.equal(video.stats.decodedOverflowDroppedFrames, 0);
        assert.equal(video.stats.decoderResetDroppedFrames, 0);
        assert.ok(draws.every((d) => d.freshnessMs >= 0));
        if (scenario === "stable" && mode === 100) {
          assert.ok(
            steady.every((d) => d.freshnessMs >= 100 && d.freshnessMs <= 117),
          );
          // Target100 is capture→draw, not an extra100ms after arrival (~38ms).
          assert.ok(distribution(steady.map((d) => d.freshnessMs)).p95! < 138);
        }
        if (scenario === "asymmetric")
          assert.equal(samples.at(-1)!.offsetErrorMs, 20);
        output.push({
          scenario,
          mode,
          synthetic: true,
          durationMs: duration,
          input: {
            fps: 60,
            width: 1920,
            height: 1080,
            chroma: "RGB",
            bitrateKbps: 16000,
            encodeDelayMs: 8,
            decoderBatchDelayMs: 4,
            drawHz: 60,
            serverOffsetMs,
            path: "stable30/30; step70/70 at5s; asymmetric10/50; jitterdown30+[0,0,15,50,0,90,5],TCP ordered",
          },
          firstDrawMs: draws[0]?.atMs,
          confidence,
          samples,
          summary: {
            freshnessMs: distribution(steady.map((d) => d.freshnessMs)),
            latenessMs: distribution(steady.map((d) => d.latenessMs)),
            drawIntervalMs: distribution(
              steady.slice(1).map((d, i) => d.atMs - steady[i]!.atMs),
            ),
            confidentFraction:
              steady.filter((d) => d.confident).length / steady.length,
            drawnFps: (steady.length * 1000) / (duration - 3000),
            stats: video.stats,
          },
          draws,
        });
      } finally {
        video.disconnect();
      }
      assert.equal(
        new Set(installed.closedTimestamps).size,
        installed.closedTimestamps.length,
      );
    });
  }

after(() => {
  if (process.env.PLAYBACK_REPLAY_OUT)
    writeFileSync(process.env.PLAYBACK_REPLAY_OUT, JSON.stringify(output));
});
