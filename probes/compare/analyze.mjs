import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { distribution, gaps, summarize } from "./metrics.mjs";

const paths = process.argv.slice(2);
assert(
  paths.length > 0,
  "Usage: node probes/compare/analyze.mjs recording.json [...]",
);
for (const path of paths) {
  const record = JSON.parse(await readFile(path, "utf8"));
  assert.equal(record.schema, 1);
  assert.deepEqual(
    summarize(record),
    record.summary,
    "Saved summary differs from raw data",
  );
  const centralFrames = record.canvasUpdateFramesMs.filter(
    (time) => time >= 2000 && time < 28000,
  );
  const sdk = {};
  for (const key of [
    "renderedFps",
    "rttMs",
    "clockUncertaintyMs",
    "latencyTargetMs",
    "latenessMs",
    "pendingInputCount",
    "decoderQueue",
    "droppedFrames",
    "bitrateKbps",
    "scalePercent",
  ]) {
    const values = record.sdkStats
      .map((sample) => sample[key])
      .filter(Number.isFinite);
    if (values.length)
      sdk[key] = {
        first: values[0],
        last: values.at(-1),
        min: Math.min(...values),
        ...distribution(values),
      };
  }
  const largeGaps = record.canvasUpdateFramesMs
    .slice(1)
    .map((time, index) => ({
      fromMs: record.canvasUpdateFramesMs[index],
      toMs: time,
      durationMs: time - record.canvasUpdateFramesMs[index],
    }))
    .filter((gap) => gap.durationMs > 100);
  console.log(
    JSON.stringify(
      {
        file: path,
        transport: record.transport,
        workload: record.workload,
        startedAt: record.startedAt,
        browser: record.browser,
        hardwareConcurrency: record.hardwareConcurrency,
        dimensions: record.dimensions,
        finalDimensions: record.finalDimensions,
        summary: record.summary,
        central2To28Seconds: {
          updateFramesPerSecond: centralFrames.length / 26,
          spacingMs: distribution(gaps(centralFrames)),
          gapsOver100ms: gaps(centralFrames).filter((gap) => gap > 100).length,
        },
        updatesBySecond: Array.from(
          { length: Math.ceil(record.durationMs / 1000) },
          (_, second) => ({
            second,
            updates: record.canvasUpdateFramesMs.filter(
              (time) => time >= second * 1000 && time < (second + 1) * 1000,
            ).length,
          }),
        ),
        largestUpdateGaps: largeGaps
          .toSorted((a, b) => b.durationMs - a.durationMs)
          .slice(0, 10),
        sdkFrameSampled: sdk,
        largestSdkLateness: record.sdkStats
          .filter((sample) => Number.isFinite(sample.latenessMs))
          .toSorted((a, b) => b.latenessMs - a.latenessMs)
          .slice(0, 5)
          .map(
            ({
              atMs,
              latenessMs,
              pendingInputCount,
              rttMs,
              decoderQueue,
              droppedFrames,
            }) => ({
              atMs,
              latenessMs,
              pendingInputCount,
              rttMs,
              decoderQueue,
              droppedFrames,
            }),
          ),
        payloadBySample: record.samples.map((sample, index) => {
          const previous = record.samples[index - 1] ?? {
            atMs: 0,
            receivedBytes: 0,
          };
          return {
            fromMs: previous.atMs,
            toMs: sample.atMs,
            receivedBytes: sample.receivedBytes - previous.receivedBytes,
          };
        }),
        sdkSamples: record.sdkStats.length,
        sdkClockConfidentSamples: record.sdkStats.filter(
          (sample) => sample.clockConfident,
        ).length,
        sdkGenerations: [
          ...new Set(record.sdkStats.map((sample) => sample.generation)),
        ],
      },
      null,
      2,
    ),
  );
}
