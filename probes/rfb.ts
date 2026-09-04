import assert from "node:assert/strict";
import { once } from "node:events";
import WebSocket, { createWebSocketStream } from "ws";

// Complete the real, shared RFB 3.8 handshake. A bare socket open/close can
// count as a failed TigerVNC login and invalidate later acceptance tests.
export async function connectRFB(origin: string, token: string) {
  const socket = new WebSocket(origin.replace(/^https:/u, "wss:") + "/vnc", {
    headers: { Authorization: `Bearer ${token}`, Origin: origin },
    handshakeTimeout: 15_000,
  });
  const stream = createWebSocketStream(socket);
  stream.on("error", () => {}); // read() below reports errors during negotiation.
  const signal = AbortSignal.timeout(15_000);
  async function read(size: number): Promise<Buffer> {
    for (;;) {
      const bytes = stream.read(size) as Buffer | null;
      if (bytes) return bytes;
      if (stream.destroyed || stream.readableEnded)
        throw stream.errored ?? new Error("RFB closed during negotiation");
      await once(stream, "readable", { signal });
    }
  }
  try {
    assert.equal((await read(12)).toString(), "RFB 003.008\n");
    stream.write("RFB 003.008\n");
    const count = (await read(1))[0]!;
    assert(count > 0, "RFB rejected the connection");
    assert(
      (await read(count)).includes(1),
      "RFB did not offer no-auth security",
    );
    stream.write(Buffer.from([1]));
    assert.equal((await read(4)).readUInt32BE(), 0);
    stream.write(Buffer.from([1])); // shared ClientInit
    const init = await read(24);
    const nameLength = init.readUInt32BE(20);
    assert(nameLength < 4096);
    const name = (await read(nameLength)).toString();
    return {
      socket,
      stream,
      width: init.readUInt16BE(0),
      height: init.readUInt16BE(2),
      name,
    };
  } catch (error) {
    socket.terminate();
    throw error;
  }
}
