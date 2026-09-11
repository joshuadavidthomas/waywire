export const initialPlayoutTargetMs = 100;
const minimumTargetMs = 50;
const maximumTargetMs = 300;
const targetStepMs = 25;
const changeCooldownMs = 5_000;
const idleAfterMs = 1_500;
// Allow one ordinary 60 Hz animation-frame wait before calling a frame late.
const lateThresholdMs = 25;
const calmThresholdMs = 17;
const busyDecodeQueue = 5;
const badStreakThreshold = 4;
const calmStreakThreshold = 120;

type Streak =
  | Readonly<{ kind: "none" }>
  | Readonly<{ kind: "bad" | "calm"; count: number }>;

export type PlayoutSample = Readonly<{
  latenessMs: number;
  decodeQueue: number;
}>;

/** A local playout ladder: brief trouble raises delay, sustained calm lowers it.
 * The caller supplies frame samples and monotonic time; no browser or timers.
 */
export class Playout {
  private targetMs = initialPlayoutTargetMs;
  private streak: Streak = { kind: "none" };
  private lastChangeMs: number | null = null;
  private lastSampleMs: number | null = null;

  // null means there is no timed frame (for example, clock synchronization is
  // incomplete). Idle gaps and mixed samples break streaks without changing delay.
  update(sample: PlayoutSample | null, nowMs: number): number {
    if (
      this.lastSampleMs !== null &&
      (nowMs - this.lastSampleMs > idleAfterMs || nowMs < this.lastSampleMs)
    )
      this.streak = { kind: "none" };
    this.lastSampleMs = nowMs;
    if (sample === null) {
      this.streak = { kind: "none" };
      return this.targetMs;
    }
    const kind =
      sample.latenessMs >= lateThresholdMs ||
      sample.decodeQueue >= busyDecodeQueue
        ? "bad"
        : sample.latenessMs <= calmThresholdMs && sample.decodeQueue <= 1
          ? "calm"
          : "none";
    if (kind === "none") {
      this.streak = { kind };
      return this.targetMs;
    }
    const threshold = kind === "bad" ? badStreakThreshold : calmStreakThreshold;
    this.streak = {
      kind,
      count:
        this.streak.kind === kind
          ? Math.min(threshold, this.streak.count + 1)
          : 1,
    };
    if (
      this.streak.count < threshold ||
      (this.lastChangeMs !== null &&
        nowMs - this.lastChangeMs < changeCooldownMs)
    )
      return this.targetMs;

    const old = this.targetMs;
    this.targetMs =
      kind === "bad"
        ? Math.min(maximumTargetMs, old + targetStepMs)
        : Math.max(minimumTargetMs, old - targetStepMs);
    this.streak = { kind: "none" };
    if (this.targetMs !== old) this.lastChangeMs = nowMs;
    return this.targetMs;
  }
}
