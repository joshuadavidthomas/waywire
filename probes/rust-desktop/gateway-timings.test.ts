import assert from "node:assert/strict";
import test from "node:test";
import {
  parseGatewayTimingTrace,
  summarizeGatewayTimingTrace,
} from "./gateway-timings.js";

const fixture = (records: unknown[], overflow_count = 0) => ({
  schema: "sprite-desktop-gateway-stage-timings-v1",
  pid: 456,
  overflow_count,
  records,
});
const at = (
  stage: string,
  timestamp: number,
  extra: Record<string, unknown>,
) => ({
  stage,
  timestamp_nanos: String(timestamp),
  ...extra,
});

test("strictly parses and correlates gateway boundaries", () => {
  const trace = parseGatewayTimingTrace(
    fixture([
      at("rtp_au_complete", 100, {
        rtp_timestamp: 9000,
        ssrc: 12,
        byte_length: 300,
      }),
      at("metadata_match", 120, {
        sequence: 7,
        generation: 2,
        rtp_timestamp: 9000,
        ssrc: 12,
      }),
      at("hub_enqueue", 130, { sequence: 7, generation: 2 }),
      at("socket_write_start", 140, {
        sequence: 7,
        generation: 2,
        connection_id: 3,
        byte_length: 340,
      }),
      at("socket_write_end", 170, {
        sequence: 7,
        generation: 2,
        connection_id: 3,
        byte_length: 340,
      }),
    ]),
  );
  const summary = summarizeGatewayTimingTrace(trace, [
    {
      stage: "pipe_write_end",
      timestamp_nanos: "90",
      sequence: 7,
      generation: 2,
    },
  ]);
  assert.match(summary.scope, /enqueue start before any subscriber/u);
  assert.equal(summary.intervals.nativePipeWriteToRtpAu.p50, 0.00001);
  assert.equal(summary.intervals.rtpAuToMetadataMatch.count, 1);
  assert.equal(summary.intervals.metadataMatchToHubEnqueue.p50, 0.00001);
  assert.equal(summary.intervals.hubEnqueueToSocketWriteStart.p50, 0.00001);
  assert.equal(summary.intervals.socketWrite.p50, 0.00003);
});

test("rejects unknown fields, unsafe values, malformed timestamps, and negative spans", () => {
  assert.throws(() => parseGatewayTimingTrace({ ...fixture([]), extra: true }));
  assert.throws(() =>
    parseGatewayTimingTrace(
      fixture([
        at("hub_enqueue", 1, { sequence: 1, generation: 1, payload: "secret" }),
      ]),
    ),
  );
  assert.throws(() =>
    parseGatewayTimingTrace(
      fixture([at("hub_enqueue", 1, { sequence: 1, generation: 0 })]),
    ),
  );
  assert.throws(() =>
    parseGatewayTimingTrace(
      fixture([
        {
          stage: "hub_enqueue",
          timestamp_nanos: 1,
          sequence: 1,
          generation: 1,
        },
      ]),
    ),
  );
  assert.throws(() =>
    summarizeGatewayTimingTrace(
      parseGatewayTimingTrace(
        fixture([
          at("socket_write_start", 20, {
            sequence: 1,
            generation: 1,
            connection_id: 1,
            byte_length: 40,
          }),
          at("socket_write_end", 10, {
            sequence: 1,
            generation: 1,
            connection_id: 1,
            byte_length: 40,
          }),
        ]),
      ),
    ),
  );
});

test("surfaces bounded overflow", () => {
  assert.equal(parseGatewayTimingTrace(fixture([], 4)).overflow_count, 4);
  assert.throws(() =>
    parseGatewayTimingTrace(fixture(new Array(65_537).fill({}))),
  );
});
