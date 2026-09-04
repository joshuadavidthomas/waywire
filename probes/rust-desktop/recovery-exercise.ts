import assert from "node:assert/strict";

export const recoveryExerciseTimeoutMs = 10_000;
export const injectedDecoderQueueSize = 24;
export const recoveryInjectionLabel =
  "injected queue-pressure observation (not actual decoder overload)" as const;
const recoveryScope =
  "One probe-only decodeQueueSize read is replaced with 24. The unchanged SDK makes the queue-cap decision and calls the real decoder reset; reset, output, and draw fields report actual browser operations." as const;

interface RecoveryHeader {
  readonly atMs: number;
  readonly sequence: string;
  readonly generation: number;
  readonly mediaTimestampMicros: string;
  readonly flags: number;
}

interface RecoveryPoint {
  readonly atMs: number;
  readonly mediaTimestampMicros: string;
}

export interface RecoveryObservation {
  readonly schema: "sprite-desktop-decoder-recovery-v1";
  readonly scope: typeof recoveryScope;
  readonly injectionLabel: typeof recoveryInjectionLabel;
  readonly injectedDecodeQueueSize: typeof injectedDecoderQueueSize;
  readonly timeoutMs: typeof recoveryExerciseTimeoutMs;
  readonly maxAttempts: 1;
  readonly attemptsStarted: 1;
  readonly finished: boolean;
  readonly timedOut: boolean;
  readonly drawHookInstalled: boolean;
  readonly triggerKeyframe: RecoveryHeader | null;
  readonly injectionDelta: RecoveryHeader | null;
  readonly injection: {
    readonly atMs: number;
    readonly actualDecodeQueueSize: number;
  } | null;
  readonly resetCount: number;
  readonly unexpectedResetsBeforeInjection: number;
  readonly reset: {
    readonly atMs: number;
    readonly actualDecodeQueueSize: number;
  } | null;
  readonly nextKeyframe: RecoveryHeader | null;
  readonly firstDecoderOutput: RecoveryPoint | null;
  readonly firstDraw: RecoveryPoint | null;
}

export interface RecoveryCheck {
  readonly name: string;
  readonly passed: boolean;
  readonly actual: unknown;
  readonly requirement: string;
}

export interface RecoveryExerciseResult {
  readonly schema: "sprite-desktop-decoder-recovery-result-v1";
  readonly passed: boolean;
  readonly explanation: string;
  readonly observation: RecoveryObservation;
  readonly checks: readonly RecoveryCheck[];
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
      value >= 0 &&
      (!integer || Number.isSafeInteger(value)),
    `${name} is invalid`,
  );
  return value;
}

function boolean(value: unknown, name: string): boolean {
  assert.equal(typeof value, "boolean", `${name} must be boolean`);
  return value as boolean;
}

function decimal(value: unknown, name: string): string {
  assert(
    typeof value === "string" &&
      value.length <= 20 &&
      /^(0|[1-9]\d*)$/u.test(value),
    `${name} must be an unsigned decimal string`,
  );
  BigInt(value);
  return value;
}

function time(value: unknown, name: string): number {
  const result = number(value, name);
  assert(
    result <= recoveryExerciseTimeoutMs,
    `${name} lies outside the recovery window`,
  );
  return result;
}

function nullable<T>(value: unknown, parse: (value: unknown) => T): T | null {
  return value === null ? null : parse(value);
}

function header(value: unknown, name: string): RecoveryHeader {
  const input = record(value, name);
  exactKeys(input, [
    "atMs",
    "sequence",
    "generation",
    "mediaTimestampMicros",
    "flags",
  ]);
  return {
    atMs: time(input.atMs, `${name} time`),
    sequence: decimal(input.sequence, `${name} sequence`),
    generation: number(input.generation, `${name} generation`, true),
    mediaTimestampMicros: decimal(
      input.mediaTimestampMicros,
      `${name} media timestamp`,
    ),
    flags: number(input.flags, `${name} flags`, true),
  };
}

function point(value: unknown, name: string): RecoveryPoint {
  const input = record(value, name);
  exactKeys(input, ["atMs", "mediaTimestampMicros"]);
  return {
    atMs: time(input.atMs, `${name} time`),
    mediaTimestampMicros: decimal(
      input.mediaTimestampMicros,
      `${name} media timestamp`,
    ),
  };
}

export function parseRecoveryObservation(value: unknown): RecoveryObservation {
  const input = record(value, "recovery observation");
  exactKeys(input, [
    "schema",
    "scope",
    "injectionLabel",
    "injectedDecodeQueueSize",
    "timeoutMs",
    "maxAttempts",
    "attemptsStarted",
    "finished",
    "timedOut",
    "drawHookInstalled",
    "triggerKeyframe",
    "injectionDelta",
    "injection",
    "resetCount",
    "unexpectedResetsBeforeInjection",
    "reset",
    "nextKeyframe",
    "firstDecoderOutput",
    "firstDraw",
  ]);
  assert.equal(input.schema, "sprite-desktop-decoder-recovery-v1");
  assert.equal(input.scope, recoveryScope);
  assert.equal(input.injectionLabel, recoveryInjectionLabel);
  assert.equal(input.injectedDecodeQueueSize, injectedDecoderQueueSize);
  assert.equal(input.timeoutMs, recoveryExerciseTimeoutMs);
  assert.equal(input.maxAttempts, 1);
  assert.equal(input.attemptsStarted, 1);

  const injection = nullable(input.injection, (value) => {
    const item = record(value, "injection");
    exactKeys(item, ["atMs", "actualDecodeQueueSize"]);
    return {
      atMs: time(item.atMs, "injection time"),
      actualDecodeQueueSize: number(
        item.actualDecodeQueueSize,
        "actual queue at injection",
        true,
      ),
    };
  });
  const reset = nullable(input.reset, (value) => {
    const item = record(value, "reset");
    exactKeys(item, ["atMs", "actualDecodeQueueSize"]);
    return {
      atMs: time(item.atMs, "reset time"),
      actualDecodeQueueSize: number(
        item.actualDecodeQueueSize,
        "actual queue at reset",
        true,
      ),
    };
  });

  return {
    schema: "sprite-desktop-decoder-recovery-v1",
    scope: recoveryScope,
    injectionLabel: recoveryInjectionLabel,
    injectedDecodeQueueSize: injectedDecoderQueueSize,
    timeoutMs: recoveryExerciseTimeoutMs,
    maxAttempts: 1,
    attemptsStarted: 1,
    finished: boolean(input.finished, "finished"),
    timedOut: boolean(input.timedOut, "timedOut"),
    drawHookInstalled: boolean(input.drawHookInstalled, "drawHookInstalled"),
    triggerKeyframe: nullable(input.triggerKeyframe, (item) =>
      header(item, "trigger keyframe"),
    ),
    injectionDelta: nullable(input.injectionDelta, (item) =>
      header(item, "injection delta"),
    ),
    injection,
    resetCount: number(input.resetCount, "reset count", true),
    unexpectedResetsBeforeInjection: number(
      input.unexpectedResetsBeforeInjection,
      "unexpected resets before injection",
      true,
    ),
    reset,
    nextKeyframe: nullable(input.nextKeyframe, (item) =>
      header(item, "next keyframe"),
    ),
    firstDecoderOutput: nullable(input.firstDecoderOutput, (item) =>
      point(item, "first decoder output"),
    ),
    firstDraw: nullable(input.firstDraw, (item) => point(item, "first draw")),
  };
}

export function recoveryExerciseComplete(
  observation: RecoveryObservation,
): boolean {
  return Boolean(
    observation.injection &&
      observation.reset &&
      observation.nextKeyframe &&
      observation.firstDecoderOutput &&
      (!observation.drawHookInstalled || observation.firstDraw),
  );
}

export function assessRecoveryExercise(value: unknown): RecoveryExerciseResult {
  const observation = parseRecoveryObservation(value);
  const trigger = observation.triggerKeyframe;
  const delta = observation.injectionDelta;
  const injection = observation.injection;
  const reset = observation.reset;
  const nextKeyframe = observation.nextKeyframe;
  const output = observation.firstDecoderOutput;
  const draw = observation.firstDraw;
  const checks: RecoveryCheck[] = [
    {
      name: "one injected queue-pressure observation fired",
      passed:
        observation.attemptsStarted === 1 &&
        injection !== null &&
        injection.actualDecodeQueueSize < injectedDecoderQueueSize,
      actual: {
        attemptsStarted: observation.attemptsStarted,
        injectedDecodeQueueSize: observation.injectedDecodeQueueSize,
        actualDecodeQueueSize: injection?.actualDecodeQueueSize ?? null,
        label: observation.injectionLabel,
      },
      requirement:
        "exactly one attempt; injected observation is 24 while the actual queue is below 24",
    },
    {
      name: "injection followed a same-generation keyframe on its first delta",
      passed:
        trigger !== null &&
        delta !== null &&
        (trigger.flags & 1) === 1 &&
        (trigger.flags & 2) === 0 &&
        (delta.flags & 3) === 0 &&
        delta.generation === trigger.generation &&
        delta.atMs >= trigger.atMs &&
        injection !== null &&
        injection.atMs >= delta.atMs,
      actual: { triggerKeyframe: trigger, injectionDelta: delta, injection },
      requirement:
        "a plain keyframe, then its same-generation first plain delta, then the injected read",
    },
    {
      name: "production decoder reset ran exactly once",
      passed:
        injection !== null &&
        reset !== null &&
        observation.resetCount === 1 &&
        observation.unexpectedResetsBeforeInjection === 0 &&
        reset.actualDecodeQueueSize < injectedDecoderQueueSize &&
        reset.atMs >= injection.atMs,
      actual: {
        resetCount: observation.resetCount,
        unexpectedResetsBeforeInjection:
          observation.unexpectedResetsBeforeInjection,
        reset,
      },
      requirement:
        "one VideoDecoder.reset after the injected queue-cap read, with no earlier recovery-phase reset",
    },
    {
      name: "decoder produced output from the next keyframe",
      passed:
        reset !== null &&
        nextKeyframe !== null &&
        output !== null &&
        (nextKeyframe.flags & 3) === 1 &&
        nextKeyframe.generation === delta?.generation &&
        nextKeyframe.atMs >= reset.atMs &&
        output.atMs >= nextKeyframe.atMs &&
        output.mediaTimestampMicros === nextKeyframe.mediaTimestampMicros,
      actual: { nextKeyframe, firstDecoderOutput: output },
      requirement:
        "next received keyframe and a later actual VideoDecoder output",
    },
    {
      name: "first post-reset frame was drawn",
      passed:
        observation.drawHookInstalled &&
        output !== null &&
        draw !== null &&
        draw.atMs >= output.atMs &&
        draw.mediaTimestampMicros === output.mediaTimestampMicros,
      actual: {
        drawHookInstalled: observation.drawHookInstalled,
        firstDraw: draw,
      },
      requirement:
        "an installed canvas draw hook and an actual draw of the recovered decoder output",
    },
    {
      name: "recovery observation finished within its bounded lifetime",
      passed: observation.finished && !observation.timedOut,
      actual: {
        finished: observation.finished,
        timedOut: observation.timedOut,
        timeoutMs: observation.timeoutMs,
        maxAttempts: observation.maxAttempts,
      },
      requirement: `finished before ${recoveryExerciseTimeoutMs} ms with at most one attempt`,
    },
  ];
  return {
    schema: "sprite-desktop-decoder-recovery-result-v1",
    passed: checks.every(({ passed }) => passed),
    explanation:
      "The probe returns one synthetic decodeQueueSize value of 24 to the unchanged SDK queue-cap check. The SDK then invokes the native VideoDecoder.reset path; all reset, output, and draw observations are actual browser operations, and the recorded actual queue values are not rewritten.",
    observation,
    checks,
  };
}
