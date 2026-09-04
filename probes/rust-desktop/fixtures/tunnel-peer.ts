#!/usr/bin/env -S node --experimental-strip-types

import assert from "node:assert/strict";
import { writeFile } from "node:fs/promises";
import { connect, createServer, type Socket } from "node:net";
import { once } from "node:events";

assert.deepEqual(process.argv.slice(2), [
  "-s",
  "sprite-desktop-rust",
  "proxy",
  "3218:8080",
]);

if (process.env.TUNNEL_FIXTURE_PID_FILE)
  await writeFile(process.env.TUNNEL_FIXTURE_PID_FILE, String(process.pid));

if (process.env.TUNNEL_FIXTURE_MODE === "exit") process.exit(23);

const sockets: Socket[] = [];
const peerAddress = process.env.TUNNEL_FIXTURE_PEER_ADDRESS;
const peerPort = Number(process.env.TUNNEL_FIXTURE_PEER_PORT);
if (peerAddress && Number.isInteger(peerPort) && peerPort > 0) {
  const count = Number(process.env.TUNNEL_FIXTURE_PEER_COUNT ?? "2");
  for (let index = 0; index < count; index += 1) {
    const socket = connect({ host: peerAddress, port: peerPort });
    sockets.push(socket);
    await once(socket, "connect");
  }
}

const server = createServer();
const host =
  process.env.TUNNEL_FIXTURE_MODE === "wrong-bind" ? "0.0.0.0" : "127.0.0.1";
server.listen(3218, host);
await once(server, "listening");

const stop = () => {
  for (const socket of sockets) socket.destroy();
  server.close(() => process.exit(0));
};
process.once(
  "SIGTERM",
  process.env.TUNNEL_FIXTURE_MODE === "ignore-term" ? () => {} : stop,
);
