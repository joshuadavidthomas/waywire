import { parseArgs } from "node:util";

import WebSocket, { type RawData } from "ws";

const { values } = parseArgs({
  options: {
    name: { type: "string", default: "josh-desktop" },
    gateway: { type: "string" },
    ticket: { type: "string" },
    web: { type: "string" },
  },
  strict: true,
});

const token = process.env.SPRITES_TOKEN;
const direct = !values.gateway && !values.web;
if (direct && !token)
  throw new Error("SPRITES_TOKEN is required for a direct tunnel check");
if (values.gateway && !values.ticket)
  throw new Error("--ticket is required with --gateway");

let url: string;
if (values.web) {
  const response = await fetch(new URL("/api/desktop/ticket", values.web));
  if (!response.ok)
    throw new Error(`Ticket request failed with HTTP ${response.status}`);
  const body: unknown = await response.json();
  if (
    typeof body !== "object" ||
    body === null ||
    !("wsUrl" in body) ||
    typeof body.wsUrl !== "string"
  ) {
    throw new Error("Ticket response did not contain wsUrl");
  }
  url = body.wsUrl;
} else if (values.gateway) {
  url = `${values.gateway}?ticket=${encodeURIComponent(values.ticket ?? "")}`;
} else {
  url = `wss://api.sprites.dev/v1/sprites/${encodeURIComponent(values.name)}/proxy`;
}

await new Promise<void>((resolve, reject) => {
  const socket = new WebSocket(
    url,
    direct ? { headers: { Authorization: `Bearer ${token}` } } : undefined,
  );
  let settled = false;
  let acknowledgementSeen = false;
  let stage: "version" | "security-types" | "security-result" | "server-init" =
    "version";
  let rfbBuffer = Buffer.alloc(0);

  const timeout = setTimeout(() => {
    socket.terminate();
    fail(new Error("Timed out during the RFB handshake"));
  }, 10_000);

  const fail = (error: Error) => {
    if (settled) return;
    settled = true;
    clearTimeout(timeout);
    reject(error);
  };

  const complete = () => {
    if (settled) return;
    settled = true;
    clearTimeout(timeout);
    socket.close(1000, "Sanity check complete");
    console.log(`Completed an RFB 003.008 handshake with ${values.name}.`);
    resolve();
  };

  const handleRfbData = (bytes: Buffer) => {
    rfbBuffer = Buffer.concat([rfbBuffer, bytes]);

    while (true) {
      if (stage === "version") {
        if (rfbBuffer.length < 12) return;
        const banner = rfbBuffer.subarray(0, 12).toString("ascii");
        rfbBuffer = rfbBuffer.subarray(12);
        if (banner !== "RFB 003.008\n") {
          fail(
            new Error(
              `Expected RFB 003.008 banner, received ${JSON.stringify(banner)}`,
            ),
          );
          socket.close();
          return;
        }
        socket.send(Buffer.from("RFB 003.008\n", "ascii"));
        stage = "security-types";
        continue;
      }

      if (stage === "security-types") {
        if (rfbBuffer.length < 1) return;
        const count = rfbBuffer.readUInt8(0);
        if (rfbBuffer.length < count + 1) return;
        const types = rfbBuffer.subarray(1, count + 1);
        rfbBuffer = rfbBuffer.subarray(count + 1);
        if (!types.includes(1)) {
          fail(new Error("RFB server did not offer the None security type"));
          socket.close();
          return;
        }
        socket.send(Uint8Array.of(1));
        stage = "security-result";
        continue;
      }

      if (stage === "security-result") {
        if (rfbBuffer.length < 4) return;
        const result = rfbBuffer.readUInt32BE(0);
        rfbBuffer = rfbBuffer.subarray(4);
        if (result !== 0) {
          fail(
            new Error(`RFB security negotiation failed with status ${result}`),
          );
          socket.close();
          return;
        }
        socket.send(Uint8Array.of(1));
        stage = "server-init";
        continue;
      }

      if (rfbBuffer.length < 24) return;
      const nameLength = rfbBuffer.readUInt32BE(20);
      if (rfbBuffer.length < 24 + nameLength) return;
      complete();
      return;
    }
  };

  socket.once("open", () => {
    socket.send(JSON.stringify({ host: "localhost", port: 5900 }));
  });
  socket.on("message", (data: RawData, isBinary: boolean) => {
    const bytes = Array.isArray(data) ? Buffer.concat(data) : Buffer.from(data);

    if (!isBinary && !acknowledgementSeen) {
      const text = bytes.toString("utf8");
      let acknowledgement: unknown;
      try {
        acknowledgement = JSON.parse(text);
      } catch {
        fail(new Error("Proxy acknowledgement was not valid JSON"));
        socket.close();
        return;
      }

      if (
        typeof acknowledgement !== "object" ||
        acknowledgement === null ||
        !("status" in acknowledgement) ||
        acknowledgement.status !== "connected" ||
        !("target" in acknowledgement) ||
        typeof acknowledgement.target !== "string" ||
        !acknowledgement.target.endsWith(":5900")
      ) {
        fail(new Error(`Proxy rejected the target: ${text}`));
        socket.close();
        return;
      }

      acknowledgementSeen = true;
      console.log(`Proxy acknowledgement: ${text}`);
      return;
    }

    if (!isBinary) {
      fail(
        new Error("Received an unexpected text frame during the RFB handshake"),
      );
      socket.close();
      return;
    }
    handleRfbData(bytes);
  });
  socket.once("unexpected-response", (_request, response) => {
    fail(
      new Error(`WebSocket upgrade failed with HTTP ${response.statusCode}`),
    );
  });
  socket.once("error", fail);
});
