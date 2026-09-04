import assert from "node:assert/strict";

const schema = "sprite-desktop-gateway-stage-timings-v1";
const stages = [
  "rtp_au_complete",
  "metadata_match",
  "hub_enqueue",
  "socket_write_start",
  "socket_write_end",
] as const;
type Stage = (typeof stages)[number];

export interface GatewayTimingRecord {
  readonly stage: Stage;
  readonly timestamp_nanos: string;
  readonly sequence?: number;
  readonly generation?: number;
  readonly rtp_timestamp?: number;
  readonly ssrc?: number;
  readonly connection_id?: number;
  readonly byte_length?: number;
}
export interface GatewayTimingTrace {
  readonly schema: typeof schema;
  readonly pid: number;
  readonly overflow_count: number;
  readonly records: readonly GatewayTimingRecord[];
}
interface Distribution {
  readonly count: number;
  readonly p50: number | null;
  readonly p95: number | null;
  readonly p99: number | null;
  readonly max: number | null;
}

function exactKeys(value: Record<string, unknown>, keys: readonly string[]) {
  assert.deepEqual(Object.keys(value).toSorted(), [...keys].toSorted());
}
function integer(value: unknown, name: string, positive = false): number {
  assert(
    Number.isSafeInteger(value) && Number(value) >= (positive ? 1 : 0),
    `${name} is invalid`,
  );
  return Number(value);
}
function decimal(value: unknown, name: string): string {
  assert(
    typeof value === "string" && /^(0|[1-9]\d*)$/u.test(value),
    `${name} must be a decimal string`,
  );
  BigInt(value);
  return value;
}

export function parseGatewayTimingTrace(value: unknown): GatewayTimingTrace {
  assert(
    value && typeof value === "object" && !Array.isArray(value),
    "gateway trace must be an object",
  );
  const input = value as Record<string, unknown>;
  exactKeys(input, ["schema", "pid", "overflow_count", "records"]);
  assert.equal(input.schema, schema, "unexpected gateway trace schema");
  const pid = integer(input.pid, "PID", true);
  const overflow_count = integer(input.overflow_count, "overflow count");
  assert(
    Array.isArray(input.records) && input.records.length <= 65_536,
    "invalid gateway record count",
  );
  const records = input.records.map((item, index): GatewayTimingRecord => {
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
    const common = ["stage", "timestamp_nanos"];
    const keys =
      stage === "rtp_au_complete"
        ? [...common, "rtp_timestamp", "ssrc", "byte_length"]
        : stage === "metadata_match"
          ? [...common, "sequence", "generation", "rtp_timestamp", "ssrc"]
          : stage === "hub_enqueue"
            ? [...common, "sequence", "generation"]
            : [
                ...common,
                "sequence",
                "generation",
                "connection_id",
                "byte_length",
              ];
    exactKeys(record, keys);
    const result: GatewayTimingRecord = {
      stage,
      timestamp_nanos: decimal(
        record.timestamp_nanos,
        `record ${index} timestamp`,
      ),
    };
    if ("sequence" in record)
      Object.assign(result, {
        sequence: integer(record.sequence, `record ${index} sequence`),
      });
    if ("generation" in record)
      Object.assign(result, {
        generation: integer(
          record.generation,
          `record ${index} generation`,
          true,
        ),
      });
    if ("rtp_timestamp" in record)
      Object.assign(result, {
        rtp_timestamp: integer(
          record.rtp_timestamp,
          `record ${index} RTP timestamp`,
        ),
      });
    if ("ssrc" in record)
      Object.assign(result, {
        ssrc: integer(record.ssrc, `record ${index} SSRC`),
      });
    if ("connection_id" in record)
      Object.assign(result, {
        connection_id: integer(
          record.connection_id,
          `record ${index} connection ID`,
          true,
        ),
      });
    if ("byte_length" in record)
      Object.assign(result, {
        byte_length: integer(
          record.byte_length,
          `record ${index} byte length`,
          true,
        ),
      });
    return result;
  });
  return { schema, pid, overflow_count, records };
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

export function summarizeGatewayTimingTrace(
  trace: GatewayTimingTrace,
  nativeRecords: readonly {
    stage: string;
    timestamp_nanos: string;
    sequence: number;
    generation: number;
  }[] = [],
) {
  const au = new Map<string, bigint>();
  const frameStages = new Map<Stage, Map<string, bigint>>();
  for (const stage of stages) frameStages.set(stage, new Map());
  for (const record of trace.records) {
    const timestamp = BigInt(record.timestamp_nanos);
    if (record.stage === "rtp_au_complete") {
      const key = `${record.ssrc}:${record.rtp_timestamp}`;
      assert(!au.has(key), `duplicate RTP AU ${key}`);
      au.set(key, timestamp);
      continue;
    }
    const frame = `${record.generation}:${record.sequence}`;
    const key =
      record.connection_id === undefined
        ? frame
        : `${frame}:${record.connection_id}`;
    const target = frameStages.get(record.stage)!;
    assert(!target.has(key), `duplicate ${record.stage} ${key}`);
    target.set(key, timestamp);
  }
  const metadata = frameStages.get("metadata_match")!;
  const values = (
    starts: Map<string, bigint>,
    ends: Map<string, bigint>,
    name: string,
  ) => {
    const durations: number[] = [];
    for (const [key, start] of starts) {
      const end = ends.get(key);
      if (end !== undefined) {
        assert(end >= start, `${name} is negative for ${key}`);
        durations.push(Number(end - start) / 1e6);
      }
    }
    return distribution(durations);
  };
  const nativePipeWrites = new Map<string, bigint>(
    nativeRecords
      .filter(({ stage }) => stage === "pipe_write_end")
      .map(
        (record) =>
          [
            `${record.generation}:${record.sequence}`,
            BigInt(record.timestamp_nanos),
          ] as const,
      ),
  );
  const nativePipeWriteToAu: number[] = [];
  const auToMetadata: number[] = [];
  for (const record of trace.records)
    if (record.stage === "metadata_match") {
      const auComplete = au.get(`${record.ssrc}:${record.rtp_timestamp}`);
      if (auComplete !== undefined) {
        const metadataMatch = BigInt(record.timestamp_nanos);
        assert(
          metadataMatch >= auComplete,
          "RTP AU to metadata match is negative",
        );
        auToMetadata.push(Number(metadataMatch - auComplete) / 1e6);
        const pipeWrite = nativePipeWrites.get(
          `${record.generation}:${record.sequence}`,
        );
        if (pipeWrite !== undefined) {
          assert(
            auComplete >= pipeWrite,
            "native pipe write to RTP AU is negative",
          );
          nativePipeWriteToAu.push(Number(auComplete - pipeWrite) / 1e6);
        }
      }
    }
  const writeStartsByFrame = new Map<string, bigint>();
  for (const [key, timestamp] of frameStages.get("socket_write_start")!) {
    const frame = key.split(":", 2).join(":");
    const old = writeStartsByFrame.get(frame);
    if (old === undefined || timestamp < old)
      writeStartsByFrame.set(frame, timestamp);
  }
  return {
    schema: 1,
    scope:
      "Header-only gateway timings on the same host CLOCK_MONOTONIC as the native v2 trace. Hub enqueue marks enqueue start before any subscriber can observe the frame. Socket write completion means the async socket accepted the message; it does not prove network delivery. Native pipe-write to RTP AU completion includes encoding and pipe delivery.",
    recordCount: trace.records.length,
    overflowCount: trace.overflow_count,
    intervals: {
      nativePipeWriteToRtpAu: distribution(nativePipeWriteToAu),
      rtpAuToMetadataMatch: distribution(auToMetadata),
      metadataMatchToHubEnqueue: values(
        metadata,
        frameStages.get("hub_enqueue")!,
        "metadata match to hub enqueue",
      ),
      hubEnqueueToSocketWriteStart: values(
        frameStages.get("hub_enqueue")!,
        writeStartsByFrame,
        "hub enqueue to socket write start",
      ),
      socketWrite: values(
        frameStages.get("socket_write_start")!,
        frameStages.get("socket_write_end")!,
        "socket write",
      ),
    },
  };
}
