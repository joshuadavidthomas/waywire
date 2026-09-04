import assert from "node:assert/strict";

export const frameCounterConfiguration = {
  bits: 16,
  cellSize: 8,
  cellCount: 36,
  minimumContrast: 96,
  minimumMargin: 24,
} as const;

export const frameCounterStatus = {
  valid: 0,
  invalidGuards: 1,
  invalidComplement: 2,
  missingTimestamp: 3,
  unreadableRoi: 4,
} as const;

export type FrameCounterStatus =
  (typeof frameCounterStatus)[keyof typeof frameCounterStatus];

export interface FrameCounterRecord {
  readonly atMs: number;
  readonly mediaTimestampMicros: number;
  readonly frameId: number;
  readonly status: FrameCounterStatus;
}

export interface FrameCounterTrace {
  readonly schema: "sprite-desktop-frame-counter-v1";
  readonly drawHookInstalled: boolean;
  readonly records: readonly FrameCounterRecord[];
  readonly overflow: number;
}

const maximumRecords = 10_000;
const maximumFrameId = 2 ** frameCounterConfiguration.bits - 1;

/** Cells are: white/black guards, 16 value/inverse pairs, black/white guards. */
export function packFrameCounterCells(frameId: number): readonly number[] {
  assert(
    Number.isSafeInteger(frameId) && frameId >= 0 && frameId <= maximumFrameId,
    "frame ID is outside the 16-bit counter range",
  );
  const cells = [1, 0];
  for (let bit = 0; bit < frameCounterConfiguration.bits; bit += 1) {
    const value = (frameId >> bit) & 1;
    cells.push(value, 1 - value);
  }
  cells.push(0, 1);
  return cells;
}

export function decodeFrameCounterCells(luminance: readonly number[]): {
  readonly status: FrameCounterStatus;
  readonly frameId: number;
} {
  if (
    luminance.length !== frameCounterConfiguration.cellCount ||
    luminance.some(
      (value) => !Number.isFinite(value) || value < 0 || value > 255,
    )
  ) {
    return { status: frameCounterStatus.unreadableRoi, frameId: -1 };
  }
  const white = (luminance[0]! + luminance[35]!) / 2;
  const black = (luminance[1]! + luminance[34]!) / 2;
  if (white - black < frameCounterConfiguration.minimumContrast) {
    return { status: frameCounterStatus.invalidGuards, frameId: -1 };
  }
  const threshold = (white + black) / 2;
  const classify = (value: number): number | null => {
    if (Math.abs(value - threshold) < frameCounterConfiguration.minimumMargin)
      return null;
    return value > threshold ? 1 : 0;
  };
  if (
    classify(luminance[0]!) !== 1 ||
    classify(luminance[1]!) !== 0 ||
    classify(luminance[34]!) !== 0 ||
    classify(luminance[35]!) !== 1
  ) {
    return { status: frameCounterStatus.invalidGuards, frameId: -1 };
  }
  let frameId = 0;
  for (let bit = 0; bit < frameCounterConfiguration.bits; bit += 1) {
    const value = classify(luminance[2 + bit * 2]!);
    const inverse = classify(luminance[3 + bit * 2]!);
    if (value === null || inverse === null || value === inverse) {
      return { status: frameCounterStatus.invalidComplement, frameId: -1 };
    }
    frameId += value * 2 ** bit;
  }
  return { status: frameCounterStatus.valid, frameId };
}

export function frameCounterLavfiInput(): string {
  const { cellSize, cellCount, bits } = frameCounterConfiguration;
  const filters = [
    "testsrc2=size=1824x848:rate=60",
    `drawbox=x=0:y=0:w=${cellSize * cellCount}:h=${cellSize}:color=black:t=fill`,
    `drawbox=x=0:y=0:w=${cellSize}:h=${cellSize}:color=white:t=fill`,
    `drawbox=x=${(cellCount - 1) * cellSize}:y=0:w=${cellSize}:h=${cellSize}:color=white:t=fill`,
  ];
  for (let bit = 0; bit < bits; bit += 1) {
    const weight = 2 ** bit;
    filters.push(
      `drawbox=x=${(2 + bit * 2) * cellSize}:y=0:w=${cellSize}:h=${cellSize}:color=white:t=fill:enable='bitand(n\\,${weight})'`,
      `drawbox=x=${(3 + bit * 2) * cellSize}:y=0:w=${cellSize}:h=${cellSize}:color=white:t=fill:enable='not(bitand(n\\,${weight}))'`,
    );
  }
  return filters.join(",");
}

function object(value: unknown, name: string): Record<string, unknown> {
  assert(
    value !== null && typeof value === "object" && !Array.isArray(value),
    `${name} must be an object`,
  );
  return value as Record<string, unknown>;
}

function exactKeys(value: Record<string, unknown>, keys: readonly string[]) {
  assert.deepEqual(Object.keys(value).toSorted(), [...keys].toSorted());
}

function integer(value: unknown, name: string, minimum = 0): number {
  assert(
    typeof value === "number" &&
      Number.isSafeInteger(value) &&
      value >= minimum,
    `${name} is invalid`,
  );
  return value;
}

export function parseFrameCounterTrace(value: unknown): FrameCounterTrace {
  const input = object(value, "frame counter trace");
  exactKeys(input, ["schema", "drawHookInstalled", "records", "overflow"]);
  assert.equal(
    input.schema,
    "sprite-desktop-frame-counter-v1",
    "unexpected frame counter schema",
  );
  assert.equal(typeof input.drawHookInstalled, "boolean");
  assert(
    Array.isArray(input.records) && input.records.length <= maximumRecords,
  );
  let previousAt = -1;
  const records = input.records.map((item, index) => {
    const record = object(item, `frame counter record ${index}`);
    exactKeys(record, ["atMs", "mediaTimestampMicros", "frameId", "status"]);
    assert(
      typeof record.atMs === "number" &&
        Number.isFinite(record.atMs) &&
        record.atMs >= 0 &&
        record.atMs <= 60_000 &&
        record.atMs >= previousAt,
      `frame counter record ${index} time is invalid`,
    );
    previousAt = record.atMs;
    const status = integer(
      record.status,
      `frame counter record ${index} status`,
    );
    assert(
      Object.values(frameCounterStatus).includes(status as FrameCounterStatus),
      `frame counter record ${index} has an unknown status`,
    );
    const mediaTimestampMicros = integer(
      record.mediaTimestampMicros,
      `frame counter record ${index} media timestamp`,
      -1,
    );
    const frameId = integer(
      record.frameId,
      `frame counter record ${index} frame ID`,
      -1,
    );
    if (status === frameCounterStatus.valid) {
      assert(
        mediaTimestampMicros >= 0,
        "valid counter sample lacks a timestamp",
      );
      assert(frameId >= 0 && frameId <= maximumFrameId);
    } else {
      assert.equal(frameId, -1, "invalid counter sample has a frame ID");
      assert.equal(
        mediaTimestampMicros === -1,
        status === frameCounterStatus.missingTimestamp,
        "missing timestamp status and value disagree",
      );
    }
    return {
      atMs: record.atMs,
      mediaTimestampMicros,
      frameId,
      status: status as FrameCounterStatus,
    };
  });
  return {
    schema: "sprite-desktop-frame-counter-v1",
    drawHookInstalled: input.drawHookInstalled as boolean,
    records,
    overflow: integer(input.overflow, "frame counter overflow"),
  };
}

export function summarizeFrameCounter(
  trace: FrameCounterTrace,
  recordingDurationMs: number,
) {
  assert(
    Number.isFinite(recordingDurationMs) && recordingDurationMs > 0,
    "recording duration must be positive",
  );
  const valid = trace.records.filter(
    ({ status }) => status === frameCounterStatus.valid,
  );
  const seen = new Set<number>();
  let duplicateDraws = 0;
  let sourceFrameIdGaps = 0;
  let sourceFrameIdRegressions = 0;
  let previousDistinct: number | undefined;
  for (const { frameId } of valid) {
    if (seen.has(frameId)) {
      duplicateDraws += 1;
      continue;
    }
    seen.add(frameId);
    if (previousDistinct !== undefined) {
      const delta =
        (frameId - previousDistinct + maximumFrameId + 1) %
        (maximumFrameId + 1);
      if (delta > (maximumFrameId + 1) / 2) sourceFrameIdRegressions += 1;
      else sourceFrameIdGaps += Math.max(0, delta - 1);
    }
    previousDistinct = frameId;
  }
  const invalidByStatus = Object.fromEntries(
    Object.entries(frameCounterStatus)
      .filter(([name]) => name !== "valid")
      .map(([name, status]) => [
        name,
        trace.records.filter((record) => record.status === status).length,
      ]),
  );
  return {
    scope:
      "Distinct IDs count source images delivered to the viewer draw canvas. They do not prove compositor presentation. ID gaps are missing FFmpeg source IDs between distinct draws.",
    observedDraws: trace.records.length + trace.overflow,
    recordedDraws: trace.records.length,
    validDraws: valid.length,
    invalidDraws: trace.records.length - valid.length,
    invalidByStatus,
    overflow: trace.overflow,
    distinctFrameIds: seen.size,
    duplicateDraws,
    sourceFrameIdGaps,
    sourceFrameIdRegressions,
    recordingDurationMs,
    distinctDrawnFps: (seen.size * 1000) / recordingDurationMs,
  };
}
