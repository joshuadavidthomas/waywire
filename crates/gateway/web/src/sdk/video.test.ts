import assert from "node:assert/strict";
import test from "node:test";

import {
  PROTOCOL_VERSION,
  ProtocolVersionMismatchError,
  parseVideoConfiguration,
} from "./messages.ts";
import { WaywireSession } from "./session.ts";
import {
  FakeTarget,
  FakeWebSocket,
  flush,
  installBrowser,
  installDelayedVideoDecoder,
  installQueueVideoDecoder,
  socket,
  surfaceOptions,
  videoPacket,
} from "./test-support.ts";

type VideoFixture = {
  readonly session: WaywireSession;
  readonly videoSocket: FakeWebSocket;
  readonly socketAttempts: Array<{ path: string; socket: FakeWebSocket }>;
  readonly draws: number[];
};

async function videoFixture(): Promise<VideoFixture> {
  installBrowser();
  const sockets = new Map<string, FakeWebSocket>();
  const socketAttempts: Array<{ path: string; socket: FakeWebSocket }> = [];
  const draws: number[] = [];
  const canvas = new FakeTarget();
  canvas.getContext = () => ({
    drawImage(frame: { timestamp: number }) {
      draws.push(frame.timestamp);
    },
  });
  const session = new WaywireSession({
    endpoint: "https://desktop.example.com",
    createWebSocket(path) {
      const created = new FakeWebSocket();
      sockets.set(path, created);
      socketAttempts.push({ path, socket: created });
      return socket(created);
    },
  });
  session.attachSurface(surfaceOptions(canvas));
  session.connect();
  await flush();
  const videoSocket = sockets.get("/stream");
  if (!videoSocket) throw new Error("video socket was not opened");
  return { session, videoSocket, socketAttempts, draws };
}

function configure(socket: FakeWebSocket, codec = "avc1.42E01E"): void {
  socket.dispatch("message", {
    data: JSON.stringify({
      type: "video-config",
      version: PROTOCOL_VERSION,
      codec,
    }),
  });
}

test("protocol mismatch is terminal for both sockets", async () => {
  assert.equal(
    parseVideoConfiguration({ type: "video-config", codec: "avc1.42E01E" }),
    null,
  );
  const fixture = await videoFixture();
  const errors: Error[] = [];
  fixture.session.on("error", (error) => errors.push(error));
  const socketAttemptCount = fixture.socketAttempts.length;

  fixture.videoSocket.dispatch("message", {
    data: JSON.stringify({
      type: "video-config",
      version: PROTOCOL_VERSION + 1,
      codec: "avc1.42E01E",
    }),
  });
  await flush();

  assert.equal(fixture.session.state.video.state, "error");
  assert.equal(
    fixture.session.state.video.message,
    `server speaks protocol version ${PROTOCOL_VERSION + 1}, this page speaks ${PROTOCOL_VERSION}`,
  );
  assert.equal(errors.length, 1);
  const [error] = errors;
  assert.ok(error instanceof ProtocolVersionMismatchError);
  assert.equal(error.expected, PROTOCOL_VERSION);
  assert.equal(error.actual, PROTOCOL_VERSION + 1);
  assert.deepEqual(fixture.videoSocket.closes[0], {
    code: 4002,
    reason: "protocol version mismatch",
  });
  assert.equal(fixture.socketAttempts.length, socketAttemptCount);
  await fixture.session.dispose();
});

test("keyframes wait for decoder configuration and old sessions are ignored", async () => {
  const decoder = installDelayedVideoDecoder();
  const fixture = await videoFixture();
  configure(fixture.videoSocket);
  fixture.videoSocket.dispatch("message", {
    data: videoPacket(1_000, { keyframe: true }),
  });
  assert.equal(decoder.counts.decoded, 0);

  decoder.supportResolvers.shift()?.({ supported: true });
  await new Promise<void>((resolve) => setImmediate(resolve));
  assert.equal(decoder.counts.constructions, 1);
  assert.equal(decoder.counts.decoded, 1);
  assert.deepEqual(fixture.draws, [1_000]);
  assert.equal(fixture.session.state.video.message, "Streaming video");

  configure(fixture.videoSocket);
  fixture.videoSocket.dispatch("message", {
    data: videoPacket(2_000, { keyframe: true }),
  });
  fixture.session.disconnect();
  decoder.supportResolvers.shift()?.({ supported: true });
  await new Promise<void>((resolve) => setImmediate(resolve));
  assert.equal(decoder.counts.constructions, 1);
  assert.equal(decoder.counts.decoded, 1);
  await fixture.session.dispose();
});

test("chroma profile changes configure the decoder before the new generation is drawn", async () => {
  const decoder = installDelayedVideoDecoder();
  const fixture = await videoFixture();
  try {
    for (const [generation, chroma, codec] of [
      [1, 0, "avc1.F40034"],
      [2, 1, "avc1.640034"],
      [3, 0, "avc1.F40034"],
    ] as const) {
      configure(fixture.videoSocket, codec);
      fixture.videoSocket.dispatch("message", {
        data: videoPacket(generation * 1_000, {
          generation,
          chroma,
          keyframe: true,
        }),
      });
      assert.equal(decoder.counts.decoded, generation - 1);
      decoder.supportResolvers.shift()?.({ supported: true });
      await new Promise<void>((resolve) => setImmediate(resolve));
      assert.equal(decoder.counts.constructions, generation);
      assert.equal(decoder.counts.decoded, generation);
      assert.equal(decoder.configurations.at(-1)?.codec, codec);
      assert.equal(fixture.session.state.video.codec, codec);
      assert.equal(fixture.draws.at(-1), generation * 1_000);
    }
  } finally {
    await fixture.session.dispose();
  }
});

test("a superseded profile's queued packets cannot enter the new decoder", async () => {
  const decoder = installDelayedVideoDecoder();
  const fixture = await videoFixture();
  try {
    configure(fixture.videoSocket, "avc1.F40034");
    fixture.videoSocket.dispatch("message", {
      data: videoPacket(1_000, { generation: 1, chroma: 0, keyframe: true }),
    });
    configure(fixture.videoSocket, "avc1.640034");
    fixture.videoSocket.dispatch("message", {
      data: videoPacket(2_000, { generation: 2, chroma: 1, keyframe: true }),
    });
    // The new profile is supported first; the old async setup completes later.
    decoder.supportResolvers[1]?.({ supported: true });
    await new Promise<void>((resolve) => setImmediate(resolve));
    assert.deepEqual(fixture.draws, [2_000]);
    decoder.supportResolvers[0]?.({ supported: true });
    await new Promise<void>((resolve) => setImmediate(resolve));
    assert.deepEqual(fixture.draws, [2_000]);
    assert.equal(decoder.counts.decoded, 1);
    assert.equal(fixture.session.state.video.codec, "avc1.640034");
  } finally {
    await fixture.session.dispose();
  }
});

test("decode queue overflow drops stale data and waits for a keyframe", async () => {
  const installed = installQueueVideoDecoder();
  const fixture = await videoFixture();
  configure(fixture.videoSocket);
  await new Promise<void>((resolve) => setImmediate(resolve));
  const decoder = installed.decoder();
  if (!decoder) throw new Error("decoder was not installed");

  fixture.videoSocket.dispatch("message", {
    data: videoPacket(2_000, { keyframe: true }),
  });
  for (let index = 1; index < 24; index += 1) {
    fixture.videoSocket.dispatch("message", {
      data: videoPacket(2_000 + index),
    });
  }
  await flush();
  assert.equal(decoder.decodeQueueSize, 24);

  fixture.videoSocket.dispatch("message", { data: videoPacket(2_024) });
  await flush();
  assert.equal(decoder.resetCalls, 2);
  assert.equal(decoder.decodeQueueSize, 0);
  fixture.videoSocket.dispatch("message", { data: videoPacket(2_026) });
  await flush();
  assert.equal(decoder.decodeQueueSize, 0);

  fixture.videoSocket.dispatch("message", {
    data: videoPacket(2_027, { keyframe: true }),
  });
  fixture.videoSocket.dispatch("message", { data: videoPacket(2_028) });
  await flush();
  assert.equal(decoder.decodeQueueSize, 2);
  decoder.outputAll();
  await flush();
  assert.deepEqual(fixture.draws, [2_028]);
  await fixture.session.dispose();
});
