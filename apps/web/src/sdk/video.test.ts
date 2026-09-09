import assert from "node:assert/strict";
import test from "node:test";

import { WaymoteSession } from "./session.ts";
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
  readonly session: WaymoteSession;
  readonly videoSocket: FakeWebSocket;
  readonly draws: number[];
};

async function videoFixture(): Promise<VideoFixture> {
  installBrowser();
  const sockets = new Map<string, FakeWebSocket>();
  const draws: number[] = [];
  const canvas = new FakeTarget();
  canvas.getContext = () => ({
    drawImage(frame: { timestamp: number }) {
      draws.push(frame.timestamp);
    },
  });
  const session = new WaymoteSession({
    endpoint: "https://desktop.example.com",
    createWebSocket(path) {
      const created = new FakeWebSocket();
      sockets.set(path, created);
      return socket(created);
    },
  });
  session.attachSurface(surfaceOptions(canvas));
  session.connect();
  await flush();
  const videoSocket = sockets.get("/stream");
  if (!videoSocket) throw new Error("video socket was not opened");
  return { session, videoSocket, draws };
}

function configure(socket: FakeWebSocket): void {
  socket.dispatch("message", {
    data: JSON.stringify({ type: "video-config", codec: "avc1.42E01E" }),
  });
}

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
  assert.equal(decoder.resetCalls, 1);
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
