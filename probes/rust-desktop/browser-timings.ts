import assert from "node:assert/strict";

const schema = "sprite-desktop-browser-timings-v2" as const;
const limits = {
  headers: 10_000,
  decodeSubmissions: 10_000,
  decoderOutputs: 10_000,
  resets: 1_000,
  longTasks: 1_000,
} as const;

type OverflowName = keyof typeof limits;
export interface BrowserHeaderTiming {
  readonly atMs: number;
  readonly sequence: string;
  readonly generation: number;
  readonly mediaTimestampMicros: string;
  readonly captureNanos: string;
  readonly flags: number;
  readonly auBytes: number;
}
export interface BrowserMediaTiming {
  readonly atMs: number;
  readonly mediaTimestampMicros: string;
}
export interface BrowserTimingTrace {
  readonly schema: typeof schema;
  readonly recordingPerformanceStartMs: number;
  readonly headers: readonly BrowserHeaderTiming[];
  readonly decodeSubmissions: readonly (BrowserMediaTiming & {
    readonly auBytes: number;
  })[];
  readonly decoderOutputs: readonly BrowserMediaTiming[];
  readonly resets: readonly { atMs: number; queued: number }[];
  readonly longTasks: readonly { atMs: number; durationMs: number }[];
  readonly overflow: Readonly<Record<OverflowName, number>>;
}
export interface RecorderDrawTiming {
  readonly renderedMediaTimestampMicros: number;
  readonly drawCompletedAtMs: number;
}
interface Distribution {
  readonly count: number;
  readonly p50: number | null;
  readonly p95: number | null;
  readonly p99: number | null;
  readonly max: number | null;
}

function record(value: unknown, name: string): Record<string, unknown> {
  assert(
    value && typeof value === "object" && !Array.isArray(value),
    `${name} must be an object`,
  );
  return value as Record<string, unknown>;
}
function exactKeys(
  value: Record<string, unknown>,
  keys: readonly string[],
): void {
  assert.deepEqual(Object.keys(value).toSorted(), [...keys].toSorted());
}
function number(value: unknown, name: string, integer = false): number {
  assert(
    typeof value === "number" &&
      Number.isFinite(value) &&
      (!integer || Number.isSafeInteger(value)) &&
      value >= 0,
    `${name} is invalid`,
  );
  return value;
}
function atMs(value: unknown, name: string): number {
  const result = number(value, name);
  assert(result < 30_000, `${name} lies outside the trial window`);
  return result;
}
function decimal(value: unknown, name: string): string {
  assert(
    typeof value === "string" && /^(0|[1-9]\d*)$/u.test(value),
    `${name} must be an unsigned decimal string`,
  );
  BigInt(value);
  return value;
}
function array(value: unknown, name: OverflowName): unknown[] {
  assert(
    Array.isArray(value) && value.length <= limits[name],
    `${name} has an invalid length`,
  );
  return value;
}

export function parseBrowserTimingTrace(value: unknown): BrowserTimingTrace {
  const input = record(value, "browser timing trace");
  exactKeys(input, [
    "schema",
    "recordingPerformanceStartMs",
    "headers",
    "decodeSubmissions",
    "decoderOutputs",
    "resets",
    "longTasks",
    "overflow",
  ]);
  assert.equal(input.schema, schema, "unexpected browser timing schema");
  const headers = array(input.headers, "headers").map((item, index) => {
    const value = record(item, `header ${index}`);
    exactKeys(value, [
      "atMs",
      "sequence",
      "generation",
      "mediaTimestampMicros",
      "captureNanos",
      "flags",
      "auBytes",
    ]);
    return {
      atMs: atMs(value.atMs, `header ${index} time`),
      sequence: decimal(value.sequence, `header ${index} sequence`),
      generation: number(value.generation, `header ${index} generation`, true),
      mediaTimestampMicros: decimal(
        value.mediaTimestampMicros,
        `header ${index} media timestamp`,
      ),
      captureNanos: decimal(
        value.captureNanos,
        `header ${index} native capture timestamp`,
      ),
      flags: number(value.flags, `header ${index} flags`, true),
      auBytes: number(value.auBytes, `header ${index} AU bytes`, true),
    };
  });
  const media = (
    input: unknown,
    name: "decodeSubmissions" | "decoderOutputs",
  ) =>
    array(input, name).map((item, index) => {
      const value = record(item, `${name} ${index}`);
      exactKeys(
        value,
        name === "decodeSubmissions"
          ? ["atMs", "mediaTimestampMicros", "auBytes"]
          : ["atMs", "mediaTimestampMicros"],
      );
      return {
        atMs: atMs(value.atMs, `${name} ${index} time`),
        mediaTimestampMicros: decimal(
          value.mediaTimestampMicros,
          `${name} ${index} media timestamp`,
        ),
        ...(name === "decodeSubmissions"
          ? {
              auBytes: number(value.auBytes, `${name} ${index} AU bytes`, true),
            }
          : {}),
      };
    });
  const decodeSubmissions = media(
    input.decodeSubmissions,
    "decodeSubmissions",
  ) as BrowserTimingTrace["decodeSubmissions"];
  const decoderOutputs = media(input.decoderOutputs, "decoderOutputs");
  const resets = array(input.resets, "resets").map((item, index) => {
    const value = record(item, `reset ${index}`);
    exactKeys(value, ["atMs", "queued"]);
    return {
      atMs: atMs(value.atMs, `reset ${index} time`),
      queued: number(value.queued, `reset ${index} queue`, true),
    };
  });
  const longTasks = array(input.longTasks, "longTasks").map((item, index) => {
    const value = record(item, `long task ${index}`);
    exactKeys(value, ["atMs", "durationMs"]);
    return {
      atMs: atMs(value.atMs, `long task ${index} time`),
      durationMs: number(value.durationMs, `long task ${index} duration`),
    };
  });
  const overflowInput = record(input.overflow, "overflow");
  exactKeys(overflowInput, Object.keys(limits));
  const overflow = Object.fromEntries(
    Object.keys(limits).map((name) => [
      name,
      number(overflowInput[name], `${name} overflow`, true),
    ]),
  ) as Record<OverflowName, number>;
  return {
    schema,
    recordingPerformanceStartMs: number(
      input.recordingPerformanceStartMs,
      "recording performance start",
    ),
    headers,
    decodeSubmissions,
    decoderOutputs,
    resets,
    longTasks,
    overflow,
  };
}

function distribution(values: readonly number[]): Distribution {
  const sorted = values.toSorted((left, right) => left - right);
  const at = (fraction: number) =>
    sorted.length ? sorted[Math.ceil(sorted.length * fraction) - 1]! : null;
  return {
    count: sorted.length,
    p50: at(0.5),
    p95: at(0.95),
    p99: at(0.99),
    max: sorted.at(-1) ?? null,
  };
}

export function summarizeBrowserTimings(
  trace: BrowserTimingTrace,
  recorderDraws: readonly RecorderDrawTiming[],
) {
  const unique = <T extends BrowserMediaTiming>(
    values: readonly T[],
    name: string,
  ) => {
    const result = new Map<string, T>();
    for (const value of values) {
      assert(
        !result.has(value.mediaTimestampMicros),
        `duplicate ${name} media timestamp ${value.mediaTimestampMicros}`,
      );
      result.set(value.mediaTimestampMicros, value);
    }
    return result;
  };
  const headers = unique(trace.headers, "header");
  const submissions = unique(trace.decodeSubmissions, "decode submission");
  const outputs = unique(trace.decoderOutputs, "decoder output");
  const receiveToDecode: number[] = [];
  const decodeToOutput: number[] = [];
  for (const [timestamp, submission] of submissions) {
    const header = headers.get(timestamp);
    if (header) {
      assert(
        submission.atMs >= header.atMs,
        `decode precedes receipt for ${timestamp}`,
      );
      assert.equal(
        submission.auBytes,
        header.auBytes,
        `AU byte length changed for ${timestamp}`,
      );
      receiveToDecode.push(submission.atMs - header.atMs);
    }
    const output = outputs.get(timestamp);
    if (output) {
      assert(
        output.atMs >= submission.atMs,
        `decoder output precedes submission for ${timestamp}`,
      );
      decodeToOutput.push(output.atMs - submission.atMs);
    }
  }
  const draws = unique(
    recorderDraws.map((draw, index) => ({
      atMs: number(draw.drawCompletedAtMs, `recorder draw ${index} time`),
      mediaTimestampMicros: String(
        number(
          draw.renderedMediaTimestampMicros,
          `recorder draw ${index} media timestamp`,
          true,
        ),
      ),
    })),
    "recorder draw",
  );
  const outputToDraw: number[] = [];
  for (const [timestamp, output] of outputs) {
    const draw = draws.get(timestamp);
    if (!draw) continue;
    const outputAt = trace.recordingPerformanceStartMs + output.atMs;
    assert(
      draw.atMs >= outputAt,
      `draw precedes decoder output for ${timestamp}`,
    );
    outputToDraw.push(draw.atMs - outputAt);
  }
  const exactMatchedDraws = outputToDraw.length;
  return {
    schema: 2,
    scope:
      "Browser timings share the window performance clock. Decoder stages and SDK draws are joined exactly by WebCodecs media timestamp. Native capture timestamps use a different clock and are retained without subtraction.",
    counts: {
      headers: trace.headers.length,
      decodeSubmissions: trace.decodeSubmissions.length,
      decoderOutputs: trace.decoderOutputs.length,
      recorderDraws: recorderDraws.length,
      exactMatchedDraws,
      decoderOutputsNotDrawn: outputs.size - exactMatchedDraws,
      recorderDrawsWithoutDecoderOutput: draws.size - exactMatchedDraws,
      longTasks: trace.longTasks.length,
      decoderResets: trace.resets.length,
    },
    overflow: trace.overflow,
    auBytes: distribution(trace.headers.map(({ auBytes }) => auBytes)),
    receiveToDecodeSubmissionMs: distribution(receiveToDecode),
    decodeSubmissionToOutputMs: distribution(decodeToOutput),
    decoderOutputToDrawMs: distribution(outputToDraw),
    longTaskDurationMs: distribution(
      trace.longTasks.map(({ durationMs }) => durationMs),
    ),
  };
}
