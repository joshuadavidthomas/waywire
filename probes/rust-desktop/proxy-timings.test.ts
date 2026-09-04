import assert from "node:assert/strict";
import test from "node:test";
import { ProxyVideoTimings } from "./proxy-timings.js";

const MAX_AU_BYTES = 16 * 1024 * 1024;

function videoMessage(
  auBytes = 3,
  overrides: { version?: number; type?: number } = {},
): Uint8Array {
  const bytes = new Uint8Array(40 + auBytes);
  const view = new DataView(bytes.buffer);
  view.setUint8(0, overrides.version ?? 2);
  view.setUint8(1, overrides.type ?? 1);
  view.setUint8(2, 5);
  view.setBigUint64(4, 9_007_199_254_740_999n, true);
  view.setBigUint64(12, 8_000_000_000_001n, true);
  view.setUint32(20, 17, true);
  view.setBigUint64(28, 12_345_678_901_234n, true);
  bytes.fill(0xab, 40);
  return bytes;
}

function wireHeader(
  opcode: number,
  fin: boolean,
  payloadBytes: number,
  masked = false,
): Uint8Array {
  const extended = payloadBytes < 126 ? 0 : payloadBytes <= 0xffff ? 2 : 8;
  const header = new Uint8Array(2 + extended);
  header[0] = (fin ? 0x80 : 0) | opcode;
  header[1] =
    (masked ? 0x80 : 0) |
    (extended === 0 ? payloadBytes : extended === 2 ? 126 : 127);
  if (extended === 2) {
    new DataView(header.buffer).setUint16(2, payloadBytes);
  } else if (extended === 8) {
    new DataView(header.buffer).setBigUint64(2, BigInt(payloadBytes));
  }
  return header;
}

function frame(
  payload: Uint8Array,
  { opcode = 2, fin = true, masked = false } = {},
): Uint8Array {
  return join(wireHeader(opcode, fin, payload.byteLength, masked), payload);
}

function join(...parts: readonly Uint8Array[]): Uint8Array {
  const result = new Uint8Array(
    parts.reduce((length, part) => length + part.byteLength, 0),
  );
  let offset = 0;
  for (const part of parts) {
    result.set(part, offset);
    offset += part.byteLength;
  }
  return result;
}

function expectedRecord(
  firstByteAtMs: number,
  completedAtMs: number,
  auBytes = 3,
) {
  return {
    generation: 17,
    sequence: "9007199254740999",
    mediaTimestampMicros: "8000000000001",
    captureNanos: "12345678901234",
    flags: 5,
    auBytes,
    firstByteAtMs,
    completedAtMs,
  };
}

test("times a fixed v2 header split across upstream chunks", () => {
  let now = 100;
  const timings = new ProxyVideoTimings(() => now);
  const wire = frame(videoMessage());

  timings.start();
  now = 101;
  timings.receive(wire.subarray(0, 1));
  now = 104;
  timings.receive(wire.subarray(1, 15));
  now = 106;
  timings.receive(wire.subarray(15, 31));
  now = 109;
  timings.receive(wire.subarray(31));

  assert.deepEqual(timings.finish(), {
    performanceStartMs: 100,
    records: [expectedRecord(1, 9)],
    overflow: 0,
    error: null,
  });
});

test("parses 16-bit and 64-bit lengths and coalesced frames", () => {
  let now = 20;
  const timings = new ProxyVideoTimings(() => now);
  const medium = videoMessage(86); // 126 total bytes: 16-bit form.
  const large = videoMessage(65_496); // 65,536 total bytes: 64-bit form.

  timings.start();
  now = 22;
  timings.receive(join(frame(medium), frame(large)));

  assert.deepEqual(timings.finish().records, [
    expectedRecord(2, 2, 86),
    expectedRecord(2, 2, 65_496),
  ]);
});

test("reassembles binary continuations around ping, pong, and close frames", () => {
  let now = 50;
  const timings = new ProxyVideoTimings(() => now);
  const message = videoMessage(8);

  timings.start();
  now = 51;
  timings.receive(frame(message.subarray(0, 11), { fin: false }));
  now = 52;
  timings.receive(
    join(
      frame(new Uint8Array([1, 2]), { opcode: 9 }),
      frame(message.subarray(11, 29), { opcode: 0, fin: false }),
      frame(new Uint8Array(), { opcode: 10 }),
      frame(new Uint8Array(), { opcode: 8 }),
    ),
  );
  now = 57;
  timings.receive(frame(message.subarray(29), { opcode: 0 }));

  assert.deepEqual(timings.finish().records, [expectedRecord(1, 7, 8)]);
});

test("ignores text, control payload, and unknown binary message types", () => {
  let now = 1;
  const timings = new ProxyVideoTimings(() => now);
  timings.start();
  now = 2;
  timings.receive(
    join(
      frame(new TextEncoder().encode('{"token":"must not be parsed"}'), {
        opcode: 1,
      }),
      frame(new Uint8Array([9, 8, 7]), { opcode: 9 }),
      frame(videoMessage(4, { version: 3 })),
      frame(videoMessage(4, { type: 2 })),
    ),
  );
  assert.deepEqual(timings.finish().records, []);
});

test("turns malformed server wire into a static terminal error without throwing", () => {
  const oversized = wireHeader(2, true, 16 * 1024 * 1024 + 41);
  const cases = [
    frame(new Uint8Array([1]), { masked: true }),
    oversized,
    frame(new Uint8Array([1]), { opcode: 0 }),
    wireHeader(9, false, 0),
    new Uint8Array([0x83, 0]),
    new Uint8Array([0x80, 126, 0, 1]),
  ];

  for (const malformed of cases) {
    let now = 10;
    const timings = new ProxyVideoTimings(() => now);
    timings.start();
    now = 11;
    assert.doesNotThrow(() => timings.receive(malformed));
    assert.deepEqual(timings.finish(), {
      performanceStartMs: 10,
      records: [],
      overflow: 0,
      error: "malformed-websocket-server-stream",
    });

    now = 20;
    timings.start();
    timings.receive(frame(videoMessage()));
    assert.equal(
      timings.finish().error,
      "malformed-websocket-server-stream",
      "a restart must not present a terminal parser as clean",
    );
  }
});

test("keeps framing while inactive without clocks or message builders", () => {
  let now = 100;
  let clockCalls = 0;
  const timings = new ProxyVideoTimings(() => {
    clockCalls += 1;
    return now;
  });
  const message = videoMessage(1);
  const header = wireHeader(2, true, message.byteLength);

  timings.receive(frame(new TextEncoder().encode("inactive"), { opcode: 1 }));
  timings.receive(header.subarray(0, 1));
  timings.receive(header.subarray(1));
  assert.equal(clockCalls, 0);

  timings.start();
  now = 103;
  timings.receive(message.subarray(0, 20));
  now = 106;
  timings.receive(message.subarray(20));
  assert.equal(clockCalls, 3);
  now = 107;
  timings.receive(frame(message));
  assert.deepEqual(timings.finish().records, [expectedRecord(7, 7, 1)]);
});

test("skips an upgrade message whose first header byte predates sampling", () => {
  let now = 0;
  const timings = new ProxyVideoTimings(() => now);
  const wire = frame(videoMessage(2));
  timings.receive(wire.subarray(0, 1));
  timings.start();
  now = 3;
  timings.receive(wire.subarray(1));
  timings.receive(wire);
  assert.deepEqual(timings.finish().records, [expectedRecord(3, 3, 2)]);
});

test("allows a negative first-byte time when start resets an active partial message", () => {
  let now = 100;
  const timings = new ProxyVideoTimings(() => now);
  const wire = frame(videoMessage(2));

  timings.start();
  now = 101;
  timings.receive(wire.subarray(0, 22));
  now = 105;
  timings.start();
  now = 106;
  timings.receive(wire.subarray(22));

  assert.deepEqual(timings.finish().records, [expectedRecord(-4, 1, 2)]);
});

test("accepts the size limit, rejects a larger fragmented message, and retains no payload in its snapshot", () => {
  let now = 0;
  const timings = new ProxyVideoTimings(() => now);
  const messageBytes = MAX_AU_BYTES + 40;
  const header = videoMessage(0);
  const payloadChunk = new Uint8Array(1024 * 1024);

  timings.start();
  now = 1;
  timings.receive(wireHeader(2, true, messageBytes));
  timings.receive(header);
  for (let index = 0; index < 16; index += 1) timings.receive(payloadChunk);

  timings.receive(wireHeader(2, false, messageBytes));
  timings.receive(header);
  for (let index = 0; index < 16; index += 1) timings.receive(payloadChunk);
  timings.receive(frame(new Uint8Array([1]), { opcode: 0 }));

  const snapshot = timings.finish();
  assert.equal(snapshot.error, "malformed-websocket-server-stream");
  assert.deepEqual(snapshot.records, [expectedRecord(1, 1, MAX_AU_BYTES)]);
  assert.deepEqual(
    Object.keys(snapshot.records[0]!).toSorted(),
    [
      "generation",
      "sequence",
      "mediaTimestampMicros",
      "captureNanos",
      "flags",
      "auBytes",
      "firstByteAtMs",
      "completedAtMs",
    ].toSorted(),
  );
});

test("caps records at ten thousand and counts overflow", () => {
  let now = 0;
  const timings = new ProxyVideoTimings(() => now);
  const wire = frame(videoMessage(0));

  timings.start();
  now = 1;
  for (let index = 0; index < 10_003; index += 1) timings.receive(wire);
  const snapshot = timings.finish();

  assert.equal(snapshot.records.length, 10_000);
  assert.equal(snapshot.overflow, 3);
});
