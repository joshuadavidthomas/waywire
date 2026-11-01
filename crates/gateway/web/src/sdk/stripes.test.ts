import assert from "node:assert/strict";
import test from "node:test";

import { StripeCompositor, h264Codec, parseStripes } from "./stripes.ts";
import { installEncodedVideoChunk, installGlobal } from "./test-support.ts";

test("stripe container parses each stripe and rejects malformed input", () => {
  // Matches push_stripe's layout in crates/protocol/src/browser.rs.
  const data = new Uint8Array([
    90, 0, 180, 0, 1, 2, 0, 0, 0, 0xaa, 0xbb, 0, 1, 64, 0, 0, 3, 0, 0, 0, 1, 2,
    3,
  ]);
  const stripes = parseStripes(data);
  assert.deepEqual(
    stripes?.map(({ y, height, key, data }) => [y, height, key, [...data]]),
    [
      [90, 180, true, [0xaa, 0xbb]],
      [256, 64, false, [1, 2, 3]],
    ],
  );
  assert.deepEqual(parseStripes(new Uint8Array()), []);
  assert.equal(parseStripes(data.subarray(0, data.byteLength - 1)), null);
  const badFlags = data.slice();
  badFlags[4] = 2;
  assert.equal(parseStripes(badFlags), null);
});

test("codec string comes from the sequence parameter set", () => {
  const unit = new Uint8Array([
    0, 0, 0, 1, 0x09, 0xf0, 0, 0, 0, 1, 0x67, 0x42, 0xc0, 0x1f, 0xda,
  ]);
  assert.equal(h264Codec(unit), "avc1.42C01F");
  assert.equal(h264Codec(new Uint8Array([0, 0, 0, 1, 0x65, 1, 2, 3, 4])), null);
});

test("one full-height stripe reaches playout without painting the canvas", () => {
  const decoders: Array<{ output(frame: unknown): void }> = [];
  installGlobal(
    "VideoDecoder",
    class {
      state = "unconfigured";
      decodeQueueSize = 0;
      constructor(readonly init: { output(frame: unknown): void }) {
        decoders.push(init);
      }
      configure() {
        this.state = "configured";
      }
      decode() {}
      close() {}
    },
  );
  installEncodedVideoChunk();
  const painted: number[] = [];
  installGlobal(
    "OffscreenCanvas",
    class {
      constructor(
        readonly width: number,
        readonly height: number,
      ) {}
      getContext() {
        return {
          drawImage: (_frame: unknown, _x: number, y: number) =>
            painted.push(y),
        };
      }
    },
  );
  installGlobal(
    "VideoFrame",
    class {
      constructor(
        readonly source: unknown,
        readonly init: { timestamp: number },
      ) {}
    },
  );
  // y u16, height u16, key flag u8, length u32, then an access unit whose
  // sequence parameter set names the codec.
  const stripe = (y: number, height: number) => [
    y & 0xff,
    y >> 8,
    height & 0xff,
    height >> 8,
    1,
    9,
    0,
    0,
    0,
    0,
    0,
    0,
    1,
    0x67,
    0x42,
    0xc0,
    0x1f,
    0xda,
  ];
  const decoded = (timestamp: number) => ({ timestamp, close() {} });
  const frames: unknown[] = [];
  const one = new StripeCompositor(
    (frame) => frames.push(frame),
    (error) => {
      throw error;
    },
  );
  assert.ok(one.decode(new Uint8Array(stripe(0, 720)), 7, 1280, 720));
  const whole = decoded(7);
  decoders[0]?.output(whole);
  assert.deepEqual(frames, [whole], "the decoded frame is the picture");
  assert.deepEqual(painted, []);

  const two = new StripeCompositor(
    (frame) => frames.push(frame),
    (error) => {
      throw error;
    },
  );
  assert.ok(
    two.decode(
      new Uint8Array([...stripe(0, 360), ...stripe(360, 360)]),
      8,
      1280,
      720,
    ),
  );
  decoders[1]?.output(decoded(8));
  decoders[2]?.output(decoded(8));
  assert.deepEqual(painted, [0, 360], "stripes are composited");
  assert.equal(
    (frames[1] as unknown as { init: { timestamp: number } }).init.timestamp,
    8,
  );
});
