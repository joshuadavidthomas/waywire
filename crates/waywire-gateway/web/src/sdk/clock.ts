const clockMaximumAgeMilliseconds = 5000;
const maximumClockSampleRttMilliseconds = 60_000;

export type ClockState = Readonly<{
  offsetMicros: number | null;
  bestRttMilliseconds: number;
  sampleCount: number;
  lastSampleAt: number;
}>;

export class ClockSynchronizer {
  offsetMicros: number | null = null;
  bestRttMilliseconds = Number.POSITIVE_INFINITY;
  private sampleCount = 0;
  private lastSampleAt = 0;
  /** Called after each sample, so a video worker can follow this clock. */
  onUpdate: (() => void) | null = null;

  get state(): ClockState {
    return {
      offsetMicros: this.offsetMicros,
      bestRttMilliseconds: this.bestRttMilliseconds,
      sampleCount: this.sampleCount,
      lastSampleAt: this.lastSampleAt,
    };
  }

  /**
   * Take another clock's state, where this thread's `performance.now()` reads
   * `behindMilliseconds` less than that clock's, as a worker's does when it
   * started after the page.
   */
  follow(state: ClockState, behindMilliseconds: number): void {
    this.offsetMicros =
      state.offsetMicros === null
        ? null
        : state.offsetMicros - behindMilliseconds * 1000;
    this.bestRttMilliseconds = state.bestRttMilliseconds;
    this.sampleCount = state.sampleCount;
    this.lastSampleAt = state.lastSampleAt - behindMilliseconds;
  }

  synchronized(now = performance.now()): boolean {
    return (
      this.offsetMicros !== null &&
      this.sampleCount >= 2 &&
      now - this.lastSampleAt <= clockMaximumAgeMilliseconds
    );
  }

  update(sent: number, received: number, serverNanos: string | number): void {
    const serverMicros = Number(serverNanos) / 1000;
    const sampleRtt = received - sent;
    if (
      !Number.isFinite(serverMicros) ||
      serverMicros < 0 ||
      !Number.isFinite(sampleRtt) ||
      sampleRtt < 0 ||
      sampleRtt > maximumClockSampleRttMilliseconds
    ) {
      return;
    }
    const candidateOffset = (sent + received) * 500 - serverMicros;
    if (
      this.offsetMicros === null ||
      // Once confidence has expired, an obsolete best RTT must not prevent
      // reacquisition on a slower path. Two fresh samples are still required.
      received - this.lastSampleAt > clockMaximumAgeMilliseconds
    ) {
      this.offsetMicros = candidateOffset;
      this.bestRttMilliseconds = sampleRtt;
      this.sampleCount = 1;
      this.lastSampleAt = received;
    } else if (
      sampleRtt <=
      this.bestRttMilliseconds + Math.max(2, this.bestRttMilliseconds * 0.25)
    ) {
      const correction = candidateOffset - this.offsetMicros;
      this.offsetMicros +=
        Math.abs(correction) > 100_000 ? correction : correction * 0.1;
      this.bestRttMilliseconds = Math.min(this.bestRttMilliseconds, sampleRtt);
      this.sampleCount += 1;
      this.lastSampleAt = received;
    }
    this.onUpdate?.();
  }

  expectedPresentationTime(
    captureMicros: number,
    targetLatencyMilliseconds: number,
  ): number | null {
    if (!this.synchronized() || !Number.isFinite(captureMicros)) return null;
    return this.offsetMicros === null
      ? null
      : (captureMicros + this.offsetMicros) / 1000 + targetLatencyMilliseconds;
  }
}
