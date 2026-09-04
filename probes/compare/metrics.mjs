export function distribution(values) {
  const sorted = values.filter(Number.isFinite).toSorted((a, b) => a - b);
  const at = (p) =>
    sorted.length ? sorted[Math.ceil(p * sorted.length) - 1] : null;
  return {
    count: sorted.length,
    p50: at(0.5),
    p95: at(0.95),
    p99: at(0.99),
    max: sorted.at(-1) ?? null,
  };
}

export function gaps(times) {
  return times.slice(1).map((time, index) => time - times[index]);
}

export function summarize(record) {
  const spacing = gaps(record.canvasSubmitsMs);
  const updateSpacing = gaps(record.canvasUpdateFramesMs);
  const rafSpacing = gaps(record.animationFramesMs);
  return {
    durationSeconds: record.durationMs / 1000,
    canvasUpdateFrames: record.canvasUpdateFramesMs.length,
    canvasUpdateFramesPerSecond:
      record.durationMs > 0
        ? (record.canvasUpdateFramesMs.length * 1000) / record.durationMs
        : null,
    canvasUpdateFrameSpacingMs: distribution(updateSpacing),
    canvasUpdateFrameGapsOver100ms: updateSpacing.filter((gap) => gap > 100)
      .length,
    canvasSubmits: record.canvasSubmitsMs.length,
    canvasSubmitsPerSecond:
      record.durationMs > 0
        ? (record.canvasSubmitsMs.length * 1000) / record.durationMs
        : null,
    canvasSubmitSpacingMs: distribution(spacing),
    canvasGapsOver100ms: spacing.filter((gap) => gap > 100).length,
    browserAnimationSpacingMs: distribution(rafSpacing),
    browserAnimationGapsOver100ms: rafSpacing.filter((gap) => gap > 100).length,
    longTasks: record.longTasks.length,
    longTaskTotalMs: record.longTasks.reduce(
      (sum, task) => sum + task.durationMs,
      0,
    ),
    websocketReceivedPayloadBytes: record.receivedBytes,
    websocketSentPayloadBytes: record.sentBytes,
    receivedPayloadMbps:
      record.durationMs > 0
        ? (record.receivedBytes * 8) / (record.durationMs * 1000)
        : null,
    warnings: [
      ...record.warnings,
      ...(record.canvasUpdateFramesMs.length < 2
        ? ["Too few canvas updates for spacing statistics."]
        : []),
    ],
  };
}
