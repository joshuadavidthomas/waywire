export const initialPlayoutTargetMs = 100;
const minimumTargetMs = 50;
const maximumTargetMs = 300;
const targetStepMs = 25;
const changeCooldownMs = 5_000;
// Allow one ordinary 60 Hz animation-frame wait before calling a frame late.
const lateThresholdMs = 25;
const busyDecodeQueue = 5;
const badStreakThreshold = 4;
const recoveryDelayMs = 10_000;

export type PlayoutSample = Readonly<{
  latenessMs: number;
  decodeQueue: number;
}>;

/** Four bad frames raise delay; time without bad evidence lowers it.
 * The caller supplies frame samples and idle ticks using one monotonic clock.
 */
export class Playout {
  private targetMs = initialPlayoutTargetMs;
  private badSamples = 0;
  private quietSinceMs: number | null = null;
  private lastChangeMs: number | null = null;
  private lastUpdateMs: number | null = null;

  // Damage-driven streams can be sparse or completely idle. Absence of frames
  // is not evidence of trouble: recovery uses wall-clock time since the last bad
  // sample, never a count or sum of calm samples. Null is an idle/untimed tick.
  update(sample: PlayoutSample | null, nowMs: number): number {
    if (this.lastUpdateMs !== null && nowMs < this.lastUpdateMs) {
      this.badSamples = 0;
      this.quietSinceMs = nowMs;
      this.lastChangeMs = null;
    }
    this.lastUpdateMs = nowMs;
    // With no bad sample yet, start the quiet period at the first update.
    this.quietSinceMs ??= nowMs;
    const bad =
      sample !== null &&
      (sample.latenessMs >= lateThresholdMs ||
        sample.decodeQueue >= busyDecodeQueue);
    if (bad) {
      this.quietSinceMs = nowMs;
      this.badSamples = Math.min(badStreakThreshold, this.badSamples + 1);
    } else if (sample !== null) {
      this.badSamples = 0;
    }
    if (
      this.lastChangeMs !== null &&
      nowMs - this.lastChangeMs < changeCooldownMs
    )
      return this.targetMs;

    const old = this.targetMs;
    if (bad && this.badSamples >= badStreakThreshold) {
      this.targetMs = Math.min(maximumTargetMs, old + targetStepMs);
      this.badSamples = 0;
    } else if (!bad && nowMs - this.quietSinceMs >= recoveryDelayMs) {
      this.targetMs = Math.max(minimumTargetMs, old - targetStepMs);
    }
    if (this.targetMs !== old) this.lastChangeMs = nowMs;
    return this.targetMs;
  }
}
