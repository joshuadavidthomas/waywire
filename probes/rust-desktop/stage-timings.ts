import assert from "node:assert/strict";

const schema = "sprite-desktop-streamd-stage-timings-v2";
const stages = [
  "wayland_ready",
  "capture_session_request_complete",
  "constraint_batch_complete",
  "capture_request_complete",
  // Kept as its own boundary so saved wlr-v2 traces remain readable. It is
  // not equivalent to either ext-image-copy authorization or completion.
  "capture_request_flush_complete",
  "copy_authorized",
  "copy_flush_complete",
  "submit_start",
  "submit_end",
  "pipe_write_start",
  "pipe_write_end",
  "notification_enqueue",
  "metadata_dispatch",
] as const;
type Stage = (typeof stages)[number];

export interface NativeStageTrace {
  readonly schema: typeof schema;
  readonly pid: number;
  readonly overflow_count: number;
  readonly records: readonly StageRecord[];
}

export interface StageRecord {
  readonly stage: Stage;
  readonly timestamp_nanos: string;
  readonly sequence: number;
  readonly generation: number;
  readonly protocol_ready_nanos?: string;
  readonly outcome?: "queued" | "replaced_pending";
}

interface Distribution {
  readonly count: number;
  readonly p50: number | null;
  readonly p95: number | null;
  readonly p99: number | null;
  readonly max: number | null;
}

function exactKeys(
  value: Record<string, unknown>,
  expected: readonly string[],
) {
  assert.deepEqual(Object.keys(value).toSorted(), [...expected].toSorted());
}

function decimal(value: unknown, name: string): string {
  assert(
    typeof value === "string" && /^(0|[1-9]\d*)$/u.test(value),
    `${name} must be a decimal string`,
  );
  BigInt(value);
  return value;
}

export function parseStageTrace(value: unknown): NativeStageTrace {
  assert(
    value && typeof value === "object" && !Array.isArray(value),
    "stage trace must be an object",
  );
  const input = value as Record<string, unknown>;
  exactKeys(input, ["schema", "pid", "overflow_count", "records"]);
  assert.equal(input.schema, schema, "unexpected stage trace schema");
  assert(
    Number.isSafeInteger(input.pid) && Number(input.pid) > 1,
    "invalid stage trace PID",
  );
  assert(
    Number.isSafeInteger(input.overflow_count) &&
      Number(input.overflow_count) >= 0,
    "invalid overflow count",
  );
  assert(
    Array.isArray(input.records) && input.records.length <= 32_768,
    "invalid stage record count",
  );
  const records = input.records.map((item, index): StageRecord => {
    assert(
      item && typeof item === "object" && !Array.isArray(item),
      `record ${index} must be an object`,
    );
    const record = item as Record<string, unknown>;
    assert(
      typeof record.stage === "string" &&
        (stages as readonly string[]).includes(record.stage),
      `record ${index} has an invalid stage`,
    );
    const stage = record.stage as Stage;
    exactKeys(
      record,
      stage === "wayland_ready"
        ? [
            "stage",
            "timestamp_nanos",
            "sequence",
            "generation",
            "protocol_ready_nanos",
          ]
        : stage === "submit_end"
          ? ["stage", "timestamp_nanos", "sequence", "generation", "outcome"]
          : ["stage", "timestamp_nanos", "sequence", "generation"],
    );
    assert(
      Number.isSafeInteger(record.sequence) && Number(record.sequence) >= 0,
      `record ${index} has an invalid sequence`,
    );
    assert(
      Number.isSafeInteger(record.generation) && Number(record.generation) > 0,
      `record ${index} has an invalid generation`,
    );
    if (stage === "submit_end") {
      assert(
        record.outcome === "queued" || record.outcome === "replaced_pending",
        `record ${index} has an invalid submission outcome`,
      );
    }
    return {
      stage,
      timestamp_nanos: decimal(
        record.timestamp_nanos,
        `record ${index} timestamp_nanos`,
      ),
      sequence: Number(record.sequence),
      generation: Number(record.generation),
      ...(stage === "wayland_ready"
        ? {
            protocol_ready_nanos: decimal(
              record.protocol_ready_nanos,
              `record ${index} protocol_ready_nanos`,
            ),
          }
        : {}),
      ...(stage === "submit_end"
        ? {
            outcome: record.outcome as "queued" | "replaced_pending",
          }
        : {}),
    };
  });
  return {
    schema,
    pid: Number(input.pid),
    overflow_count: Number(input.overflow_count),
    records,
  };
}

function distribution(values: readonly number[]): Distribution {
  const sorted = values.toSorted((a, b) => a - b);
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

export function summarizeStageTrace(
  trace: NativeStageTrace,
  browserFrames: readonly { sequence: string; generation: number }[] = [],
) {
  const byStage = new Map<Stage, Map<string, bigint>>();
  const protocolReady = new Map<string, bigint>();
  let repeatedSessionRequests = 0;
  let repeatedConstraintBatches = 0;
  let repeatedRequests = 0;
  let repeatedCopyAuthorizations = 0;
  let repeatedCopyFlushes = 0;
  let replacedSubmissions = 0;
  for (const stage of stages) byStage.set(stage, new Map());
  for (const record of trace.records) {
    const key = `${record.generation}:${record.sequence}`;
    const records = byStage.get(record.stage)!;
    const timestamp = BigInt(record.timestamp_nanos);
    const previous = records.get(key);
    if (previous !== undefined) {
      // Cursor and generation changes can recapture the same sequence. Keep
      // the first boundary for each attempt stage and count later observations.
      if (record.stage === "capture_session_request_complete")
        repeatedSessionRequests++;
      else if (record.stage === "constraint_batch_complete")
        repeatedConstraintBatches++;
      else if (record.stage === "capture_request_complete") repeatedRequests++;
      else if (record.stage === "copy_authorized") repeatedCopyAuthorizations++;
      else if (record.stage === "copy_flush_complete") repeatedCopyFlushes++;
      else assert.fail(`duplicate ${record.stage} record for ${key}`);
      if (timestamp < previous) records.set(key, timestamp);
      continue;
    }
    records.set(key, timestamp);
    if (record.stage === "wayland_ready")
      protocolReady.set(key, BigInt(record.protocol_ready_nanos!));
    if (record.stage === "submit_end" && record.outcome === "replaced_pending")
      replacedSubmissions++;
  }

  const metric = (
    name: string,
    starts: Map<string, bigint>,
    ends: Map<string, bigint>,
  ) => {
    const values: number[] = [];
    for (const [key, start] of starts) {
      const end = ends.get(key);
      if (end === undefined) continue;
      assert(end >= start, `${name} is negative for ${key}`);
      values.push(Number(end - start) / 1e6);
    }
    let missingStart = 0;
    for (const key of ends.keys()) if (!starts.has(key)) missingStart++;
    let missingEnd = 0;
    for (const key of starts.keys()) if (!ends.has(key)) missingEnd++;
    return {
      milliseconds: distribution(values),
      boundaryMissing: { start: missingStart, end: missingEnd },
    };
  };

  const ready = byStage.get("wayland_ready")!;
  const nextBoundary = (source: Map<string, bigint>) =>
    new Map(
      [...source].flatMap(([key, timestamp]) => {
        const [generation, sequenceText] = key.split(":");
        const sequence = Number(sequenceText);
        return sequence === Number.MAX_SAFE_INTEGER
          ? []
          : [[`${generation}:${sequence + 1}`, timestamp] as const];
      }),
    );
  const captureCadence: number[] = [];
  const deliveryCadence: number[] = [];
  for (const source of [ready, byStage.get("metadata_dispatch")!] as const) {
    const target = source === ready ? captureCadence : deliveryCadence;
    for (const [key, timestamp] of source) {
      const [generation, sequenceText] = key.split(":");
      const sequence = Number(sequenceText);
      const next = source.get(`${generation}:${sequence + 1}`);
      if (next !== undefined) {
        assert(next >= timestamp, `cadence is negative for ${key}`);
        target.push(Number(next - timestamp) / 1e6);
      }
    }
  }
  const nativeFrames = byStage.get("metadata_dispatch")!;
  let matchedBrowserFrames = 0;
  for (const frame of browserFrames) {
    assert(
      /^(0|[1-9]\d*)$/u.test(frame.sequence),
      "browser sequence must be a decimal string",
    );
    const sequence = BigInt(frame.sequence);
    if (
      sequence <= BigInt(Number.MAX_SAFE_INTEGER) &&
      nativeFrames.has(`${frame.generation}:${Number(sequence)}`)
    )
      matchedBrowserFrames++;
  }
  return {
    schema: 2,
    recordCount: trace.records.length,
    overflowCount: trace.overflow_count,
    matchedBrowserFrames,
    repeatedSessionRequests,
    repeatedConstraintBatches,
    repeatedRequests,
    repeatedCopyAuthorizations,
    repeatedCopyFlushes,
    replacedSubmissions,
    intervals: {
      protocolReadyToCallback: metric(
        "protocol ready to callback",
        protocolReady,
        ready,
      ),
      callbackToNextRequest: metric(
        "callback to next request",
        nextBoundary(ready),
        byStage.get("capture_request_complete")!,
      ),
      initialConstraintDelay: metric(
        "session request to initial constraints",
        byStage.get("capture_session_request_complete")!,
        byStage.get("constraint_batch_complete")!,
      ),
      callbackToFlush: metric(
        "callback to next copy flush",
        nextBoundary(ready),
        byStage.get("copy_flush_complete")!,
      ),
      requestToCopyAuthorization: metric(
        "request to copy authorization",
        byStage.get("capture_request_complete")!,
        byStage.get("copy_authorized")!,
      ),
      copyAuthorizationToFlush: metric(
        "copy authorization to flush",
        byStage.get("copy_authorized")!,
        byStage.get("copy_flush_complete")!,
      ),
      submit: metric(
        "submit",
        byStage.get("submit_start")!,
        byStage.get("submit_end")!,
      ),
      pipeWrite: metric(
        "pipe write",
        byStage.get("pipe_write_start")!,
        byStage.get("pipe_write_end")!,
      ),
      notificationPublishToMetadataDispatch: metric(
        "notification publish to metadata dispatch",
        byStage.get("notification_enqueue")!,
        nativeFrames,
      ),
    },
    cadence: {
      captureReadySpacingMs: distribution(captureCadence),
      metadataDeliverySpacingMs: distribution(deliveryCadence),
    },
  };
}
