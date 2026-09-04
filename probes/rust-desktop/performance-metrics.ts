export function hasExpectedQuality(
  samples: readonly { bitrateKbps: number; scalePercent: number }[],
): boolean {
  return (
    samples.length > 0 &&
    samples.every(
      (sample) => sample.bitrateKbps === 8000 && sample.scalePercent === 100,
    )
  );
}

// Include both recording edges: a stream that stops before recording ends
// must not hide its final freeze behind otherwise short frame intervals.
export function longestFrameGap(
  durationMs: number,
  updatesMs: readonly number[],
): number {
  if (!Number.isFinite(durationMs) || durationMs <= 0)
    throw new RangeError("Recording duration must be positive and finite");
  let previous = 0;
  let longest = 0;
  for (const atMs of updatesMs) {
    if (!Number.isFinite(atMs) || atMs < previous || atMs > durationMs)
      throw new RangeError("Frame times must be ordered within the recording");
    longest = Math.max(longest, atMs - previous);
    previous = atMs;
  }
  return Math.max(longest, durationMs - previous);
}
