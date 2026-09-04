import assert from "node:assert/strict";
import test from "node:test";
import { parseStageTrace, summarizeStageTrace } from "./stage-timings.js";

const record = (
  stage: string,
  timestamp: bigint,
  sequence = 7,
  extra: Record<string, unknown> = {},
) => ({
  stage,
  timestamp_nanos: timestamp.toString(),
  sequence,
  generation: 3,
  ...(stage === "submit_end" ? { outcome: "queued" } : {}),
  ...extra,
});
const fixture = (records: unknown[], overflow_count = 0) => ({
  schema: "sprite-desktop-streamd-stage-timings-v2",
  pid: 123,
  overflow_count,
  records,
});

test("correlates same-frame stages and the next-sequence request boundary", () => {
  const trace = parseStageTrace(
    fixture([
      record("capture_session_request_complete", 10n, 7),
      record("constraint_batch_complete", 90n, 7),
      record("wayland_ready", 120n, 7, { protocol_ready_nanos: "100" }),
      record("submit_start", 130n),
      record("submit_end", 150n),
      // Single-buffer capture cannot authorize sequence 8 until submit has
      // copied every pixel from sequence 7.
      record("capture_request_complete", 160n, 8),
      record("copy_authorized", 165n, 8),
      record("copy_flush_complete", 167n, 8),
      record("pipe_write_start", 180n),
      record("pipe_write_end", 210n),
      record("notification_enqueue", 220n),
      record("metadata_dispatch", 225n),
    ]),
  );
  const summary = summarizeStageTrace(trace, [
    { sequence: "7", generation: 3 },
  ]);
  assert.equal(summary.matchedBrowserFrames, 1);
  assert.deepEqual(summary.intervals.protocolReadyToCallback.milliseconds, {
    count: 1,
    p50: 0.00002,
    p95: 0.00002,
    p99: 0.00002,
    max: 0.00002,
  });
  assert.equal(summary.intervals.callbackToNextRequest.milliseconds.count, 1);
  assert.equal(summary.intervals.initialConstraintDelay.milliseconds.count, 1);
  assert.equal(summary.intervals.callbackToFlush.milliseconds.count, 1);
  assert.equal(
    summary.intervals.requestToCopyAuthorization.milliseconds.count,
    1,
  );
  assert.equal(
    summary.intervals.copyAuthorizationToFlush.milliseconds.count,
    1,
  );
  assert.equal(
    summary.intervals.notificationPublishToMetadataDispatch.milliseconds.count,
    1,
  );
});

test("retains first persistent request boundaries across same-sequence session restarts", () => {
  const summary = summarizeStageTrace(
    parseStageTrace(
      fixture([
        record("wayland_ready", 100n, 7, { protocol_ready_nanos: "90" }),
        record("capture_session_request_complete", 105n, 8),
        record("constraint_batch_complete", 108n, 8),
        record("capture_request_complete", 110n, 8),
        // An overlay change retires this session and starts the same sequence again.
        record("capture_session_request_complete", 120n, 8),
        record("constraint_batch_complete", 128n, 8),
        record("capture_request_complete", 130n, 8),
        record("copy_authorized", 135n, 8),
        record("copy_flush_complete", 137n, 8),
        record("capture_request_complete", 140n, 8),
        record("copy_authorized", 155n, 8),
        record("copy_flush_complete", 157n, 8),
      ]),
    ),
  );
  assert.equal(summary.repeatedSessionRequests, 1);
  assert.equal(summary.repeatedConstraintBatches, 1);
  assert.equal(summary.repeatedRequests, 2);
  assert.equal(summary.repeatedCopyAuthorizations, 1);
  assert.equal(summary.repeatedCopyFlushes, 1);
  assert.equal(
    summary.intervals.callbackToNextRequest.milliseconds.p50,
    0.00001,
  );
  assert.equal(summary.intervals.callbackToFlush.milliseconds.p50, 0.000037);
  assert.equal(
    summary.intervals.initialConstraintDelay.milliseconds.p50,
    0.000003,
  );
  assert.equal(
    summary.intervals.requestToCopyAuthorization.milliseconds.p50,
    0.000025,
  );
  assert.throws(
    () =>
      summarizeStageTrace(
        parseStageTrace(
          fixture([record("submit_start", 110n), record("submit_start", 120n)]),
        ),
      ),
    /duplicate submit_start/u,
  );
});

test("accepts the distinct flush boundary in saved wlr-v2 traces", () => {
  const trace = parseStageTrace(
    fixture([record("capture_request_flush_complete", 10n, 1)]),
  );
  assert.equal(trace.records[0]?.stage, "capture_request_flush_complete");
});

test("counts pending-frame replacement outcomes", () => {
  const summary = summarizeStageTrace(
    parseStageTrace(
      fixture([
        record("submit_start", 10n, 1),
        record("submit_end", 20n, 1, { outcome: "replaced_pending" }),
      ]),
    ),
  );
  assert.equal(summary.replacedSubmissions, 1);
  assert.equal(summary.intervals.submit.milliseconds.count, 1);
});

test("reports partial trace boundaries without pairing unrelated records", () => {
  const summary = summarizeStageTrace(
    parseStageTrace(
      fixture([
        record("submit_start", 10n, 1),
        record("submit_end", 20n, 2),
        record("wayland_ready", 30n, 3, { protocol_ready_nanos: "25" }),
      ]),
    ),
  );
  assert.equal(summary.intervals.submit.milliseconds.count, 0);
  assert.deepEqual(summary.intervals.submit.boundaryMissing, {
    start: 1,
    end: 1,
  });
  assert.deepEqual(summary.intervals.callbackToNextRequest.boundaryMissing, {
    start: 0,
    end: 1,
  });
});

test("surfaces overflow and rejects malformed schemas, timestamps, and spans", () => {
  assert.equal(parseStageTrace(fixture([], 1)).overflow_count, 1);
  assert.throws(() => parseStageTrace({ ...fixture([]), schema: "old" }));
  assert.throws(() =>
    parseStageTrace(
      fixture([record("submit_start", 1n, 1, { timestamp_nanos: 2 })]),
    ),
  );
  assert.throws(() =>
    parseStageTrace(
      fixture([{ ...record("submit_start", 1n), timestampNanos: "1" }]),
    ),
  );
  assert.throws(() =>
    summarizeStageTrace(
      parseStageTrace(
        fixture([
          record("pipe_write_start", 20n),
          record("pipe_write_end", 10n),
        ]),
      ),
    ),
  );
});
