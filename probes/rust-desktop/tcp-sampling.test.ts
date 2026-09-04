import assert from "node:assert/strict";
import { once } from "node:events";
import { createConnection, createServer, type Socket } from "node:net";
import { setTimeout as sleep } from "node:timers/promises";
import { test } from "node:test";
import { parseOwnedTcpInfo, startTcpSampling } from "./tcp-sampling.js";

test("TCP parser keeps counters and rejects foreign owners", () => {
  const row =
    'ESTAB 12 34 127.0.0.1:1000 192.0.2.1:443 users:(("private-name",pid=42,fd=6))\n cubic rtt:30.2/1.4 bytes_received:12000 rcv_ooopack:2 retrans:0/1';
  assert.deepEqual(parseOwnedTcpInfo(row, 42), {
    recvQueue: 12,
    sendQueue: 34,
    counters: {
      bytes_received: [12000],
      rcv_ooopack: [2],
      retrans: [0, 1],
      rtt: [30.2, 1.4],
    },
  });
  assert.throws(() => parseOwnedTcpInfo(row, 43));
  assert.throws(() => parseOwnedTcpInfo(`${row}\nextra socket`, 42));
  assert.throws(() => parseOwnedTcpInfo(row.replace("12 34", "-1 34"), 42));
});

test("samples only an owned live TCP socket and stops polling", async () => {
  let peer: Socket | undefined;
  const server = createServer((socket) => {
    peer = socket;
    socket.on("data", (chunk) => socket.write(chunk));
  });
  server.listen(0, "127.0.0.1");
  await once(server, "listening");
  const address = server.address();
  assert(address && typeof address !== "string");
  const client = createConnection(address.port, "127.0.0.1");
  try {
    await once(client, "connect");
    client.write("owned synthetic traffic");
    await once(client, "data");
    assert(client.localPort);
    const sampling = startTcpSampling([client.localPort]);
    await sleep(300);
    const snapshot = await sampling.finish();
    assert.deepEqual(snapshot.errors, []);
    assert(snapshot.samples.length >= 1);
    assert.equal(snapshot.overflow, 0);
    const count = snapshot.samples.length;
    await sleep(300);
    assert.equal(snapshot.samples.length, count);
  } finally {
    client.destroy();
    peer?.destroy();
    await new Promise<void>((resolve) => server.close(() => resolve()));
  }
});
