import assert from "node:assert/strict";
import { execFile } from "node:child_process";
import { createHash, randomBytes } from "node:crypto";
import { mkdtemp, readFile, rm } from "node:fs/promises";
import {
  createServer as createHTTPServer,
  request as requestHTTP,
  type IncomingHttpHeaders,
} from "node:http";
import { Agent, createServer as createHTTPSServer } from "node:https";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { test } from "node:test";
import { promisify } from "node:util";
import WebSocket, { WebSocketServer } from "ws";
import {
  createAcceptanceProxy,
  createRustTunnelAcceptanceProxy,
} from "./view-runtime.js";

const exec = promisify(execFile);
const browserAuthority = "127.0.0.1:3217";
const browserOrigin = `http://${browserAuthority}`;

type SeenRequest = {
  readonly kind: "http" | "websocket";
  readonly path: string;
  readonly authorization?: string;
  readonly cookie?: string;
  readonly host?: string;
  readonly origin?: string;
};

type TestServer =
  | ReturnType<typeof createHTTPServer>
  | ReturnType<typeof createHTTPSServer>;

async function listen(server: TestServer): Promise<number> {
  await new Promise<void>((resolve, reject) => {
    server.once("error", reject);
    server.listen(0, "127.0.0.1", resolve);
  });
  const address = server.address();
  assert(address && typeof address !== "string");
  return address.port;
}

async function close(server: TestServer): Promise<void> {
  if (!server.listening) return;
  await new Promise<void>((resolve, reject) =>
    server.close((error) => (error ? reject(error) : resolve())),
  );
}

async function proxyRequest(
  port: number,
  path: string,
  options: { readonly method?: string; readonly host?: string } = {},
): Promise<{
  readonly status: number;
  readonly headers: IncomingHttpHeaders;
  readonly body: string;
}> {
  return new Promise((resolve, reject) => {
    const request = requestHTTP(
      {
        host: "127.0.0.1",
        port,
        path,
        method: options.method ?? "GET",
        headers: {
          Host: options.host ?? browserAuthority,
          Authorization: "Basic browser-secret-must-not-cross",
          Cookie: "browser-secret=must-not-cross",
        },
      },
      (response) => {
        const chunks: Buffer[] = [];
        response.on("data", (chunk: Buffer) => chunks.push(chunk));
        response.on("end", () =>
          resolve({
            status: response.statusCode!,
            headers: response.headers,
            body: Buffer.concat(chunks).toString(),
          }),
        );
      },
    );
    request.on("error", reject);
    request.end();
  });
}

async function openSocket(port: number, path: string): Promise<WebSocket> {
  const socket = new WebSocket(`ws://127.0.0.1:${port}${path}`, {
    origin: browserOrigin,
    headers: {
      Host: browserAuthority,
      Authorization: "Basic browser-secret-must-not-cross",
      Cookie: "browser-secret=must-not-cross",
    },
  });
  const opened = new Promise<void>((resolve, reject) => {
    socket.once("open", resolve);
    socket.once("error", reject);
  });
  const upgraded = new Promise<void>((resolve, reject) => {
    socket.once("upgrade", (response) => {
      try {
        assert.equal(response.headers["set-cookie"], undefined);
        resolve();
      } catch (error) {
        reject(error);
      }
    });
  });
  await Promise.all([opened, upgraded]);
  return socket;
}

async function rejectedSocket(
  port: number,
  path: string,
  origin: string,
): Promise<number> {
  return new Promise((resolve, reject) => {
    const socket = new WebSocket(`ws://127.0.0.1:${port}${path}`, {
      origin,
      headers: { Host: browserAuthority },
    });
    socket.once("unexpected-response", (_request, response) => {
      const status = response.statusCode;
      response.resume();
      socket.terminate();
      assert(status !== undefined);
      resolve(status);
    });
    socket.once("open", () => {
      socket.terminate();
      reject(new Error(`${path} unexpectedly upgraded`));
    });
    socket.once("error", (error) => {
      if ((error as Error).message.includes("Unexpected server response"))
        return;
      reject(error);
    });
  });
}

test("acceptance factories enforce HTTPS identity and a bounded tunnel port", () => {
  assert.throws(
    () => createAcceptanceProxy("http://sprite.example.test", "trial-token"),
    /canonical HTTPS origin and token/u,
  );
  assert.throws(
    () => createRustTunnelAcceptanceProxy("http://sprite.example.test", 12345),
    /canonical HTTPS origin/u,
  );
  assert.throws(
    () => createRustTunnelAcceptanceProxy("https://sprite.example.test", 0),
    /valid localhost tunnel port/u,
  );
  assert.throws(
    () => createRustTunnelAcceptanceProxy("https://sprite.example.test", 1.5),
    /valid localhost tunnel port/u,
  );
});

test("Rust tunnel acceptance proxy forwards HTTP and WebSockets to localhost", async () => {
  const canonicalOrigin = "https://sprite.example.test";
  const seen: SeenRequest[] = [];
  const observed: Buffer[] = [];
  const websocketServer = new WebSocketServer({ noServer: true });
  websocketServer.on("headers", (headers) => {
    headers.push("Set-Cookie: tunnel-session=must-not-cross");
  });
  websocketServer.on("connection", (socket) => {
    socket.on("message", (data) => socket.send(data));
  });

  const upstream = createHTTPServer((request, response) => {
    seen.push({
      kind: "http",
      path: request.url!,
      authorization: request.headers.authorization,
      cookie: request.headers.cookie,
      host: request.headers.host,
      origin: request.headers.origin,
    });
    response.writeHead(200, {
      "content-type": "text/plain",
      "set-cookie": "tunnel-http-session=must-not-cross",
    });
    response.end(`tunnel ${request.url}\n`);
  });
  upstream.on("upgrade", (request, socket, head) => {
    seen.push({
      kind: "websocket",
      path: request.url!,
      authorization: request.headers.authorization,
      cookie: request.headers.cookie,
      host: request.headers.host,
      origin: request.headers.origin,
    });
    websocketServer.handleUpgrade(request, socket, head, (websocket) => {
      websocketServer.emit("connection", websocket, request);
    });
  });

  let proxy: ReturnType<typeof createRustTunnelAcceptanceProxy> | undefined;
  try {
    const tunnelPort = await listen(upstream);
    proxy = createRustTunnelAcceptanceProxy(
      canonicalOrigin,
      tunnelPort,
      () => (chunk) => observed.push(Buffer.from(chunk)),
    );
    const proxyPort = await listen(proxy.server);

    const root = await proxyRequest(proxyPort, "/");
    assert.equal(root.status, 200);
    assert.equal(root.body, "tunnel /\n");
    assert.equal(root.headers["set-cookie"], undefined);
    assert.equal(
      (await proxyRequest(proxyPort, "/assets/app_1.js")).status,
      200,
    );
    assert.equal((await proxyRequest(proxyPort, "/audio")).status, 404);
    assert.equal(
      (await proxyRequest(proxyPort, "/", { method: "POST" })).status,
      403,
    );
    assert.equal(
      (await proxyRequest(proxyPort, "/", { host: `127.0.0.1:${proxyPort}` }))
        .status,
      403,
    );

    const socket = await openSocket(proxyPort, "/stream");
    const echoed = new Promise<Buffer>((resolve, reject) => {
      socket.once("message", (data) => resolve(Buffer.from(data as Buffer)));
      socket.once("error", reject);
    });
    socket.send("tunnel websocket payload");
    assert.equal(
      (await echoed).toString(),
      "tunnel websocket payload",
      "WebSocket payload must make the localhost round trip",
    );
    assert(observed.length > 0, "Rust stream observer must see upstream bytes");
    assert.equal(
      await rejectedSocket(proxyPort, "/stream", canonicalOrigin),
      403,
    );
    assert.equal(await rejectedSocket(proxyPort, "/audio", browserOrigin), 403);

    assert.deepEqual(seen, [
      {
        kind: "http",
        path: "/",
        authorization: undefined,
        cookie: undefined,
        host: "sprite.example.test",
        origin: canonicalOrigin,
      },
      {
        kind: "http",
        path: "/assets/app_1.js",
        authorization: undefined,
        cookie: undefined,
        host: "sprite.example.test",
        origin: canonicalOrigin,
      },
      {
        kind: "websocket",
        path: "/stream",
        authorization: undefined,
        cookie: undefined,
        host: "sprite.example.test",
        origin: canonicalOrigin,
      },
    ]);
    const closed = new Promise<void>((resolve) =>
      socket.once("close", () => resolve()),
    );
    proxy.revokeCredential();
    await closed;
  } finally {
    proxy?.revokeCredential();
    proxy?.server.closeAllConnections();
    if (proxy?.server.listening) await close(proxy.server);
    for (const socket of websocketServer.clients) socket.terminate();
    await new Promise<void>((resolve) =>
      websocketServer.close(() => resolve()),
    );
    upstream.closeAllConnections();
    await close(upstream);
  }
});

test("Rust acceptance proxy fixes routes and contains browser credentials", async () => {
  const directory = await mkdtemp(join(tmpdir(), "sprite-proxy-tls-"));
  const keyPath = join(directory, "key.pem");
  const certificatePath = join(directory, "certificate.pem");
  let upstream: ReturnType<typeof createHTTPSServer> | undefined;
  let proxy: ReturnType<typeof createAcceptanceProxy> | undefined;
  let agent: Agent | undefined;
  try {
    await exec(
      "openssl",
      [
        "req",
        "-x509",
        "-newkey",
        "rsa:2048",
        "-nodes",
        "-days",
        "1",
        "-subj",
        "/CN=127.0.0.1",
        "-addext",
        "subjectAltName=IP:127.0.0.1",
        "-keyout",
        keyPath,
        "-out",
        certificatePath,
      ],
      { timeout: 15_000 },
    );
    const [key, certificate] = await Promise.all([
      readFile(keyPath),
      readFile(certificatePath),
    ]);
    const seen: SeenRequest[] = [];
    const observed: Buffer[] = [];
    let observedConnections = 0;
    const wire = Buffer.from([0x82, 4, 1, 2, 3, 4]);
    upstream = createHTTPSServer(
      { key, cert: certificate },
      (request, response) => {
        seen.push({
          kind: "http",
          path: request.url!,
          authorization: request.headers.authorization,
          cookie: request.headers.cookie,
          host: request.headers.host,
          origin: request.headers.origin,
        });
        response.writeHead(200, {
          "content-type": "text/plain",
          "set-cookie": "upstream-session=must-not-cross",
        });
        response.end(`upstream ${request.url}\n`);
      },
    );
    upstream.on("upgrade", (request, socket) => {
      seen.push({
        kind: "websocket",
        path: request.url!,
        authorization: request.headers.authorization,
        cookie: request.headers.cookie,
        host: request.headers.host,
        origin: request.headers.origin,
      });
      const key = request.headers["sec-websocket-key"];
      assert.equal(typeof key, "string");
      const accept = createHash("sha1")
        .update(`${key}258EAFA5-E914-47DA-95CA-C5AB0DC85B11`)
        .digest("base64");
      socket.write(
        Buffer.concat([
          Buffer.from(
            "HTTP/1.1 101 Switching Protocols\r\n" +
              "Connection: Upgrade\r\n" +
              "Upgrade: websocket\r\n" +
              `Sec-WebSocket-Accept: ${accept}\r\n` +
              "Set-Cookie: upstream-socket=must-not-cross\r\n\r\n",
          ),
          wire,
        ]),
      );
    });
    const upstreamPort = await listen(upstream);
    const origin = `https://127.0.0.1:${upstreamPort}`;
    agent = new Agent({ ca: certificate });
    proxy = createAcceptanceProxy(origin, "trial-token", "rust", agent, () => {
      observedConnections += 1;
      return (chunk) => observed.push(Buffer.from(chunk));
    });
    const proxyPort = await new Promise<number>((resolve, reject) => {
      proxy!.server.once("error", reject);
      proxy!.server.listen(0, "127.0.0.1", () => {
        const address = proxy!.server.address();
        assert(address && typeof address !== "string");
        resolve(address.port);
      });
    });

    const root = await proxyRequest(proxyPort, "/");
    assert.equal(root.status, 200);
    assert.equal(root.body, "upstream /\n");
    assert.equal(root.headers["set-cookie"], undefined);
    assert.deepEqual(
      await Promise.all([
        proxyRequest(proxyPort, "/healthz?probe=1"),
        proxyRequest(proxyPort, "/assets/index-Ab_19.js"),
        proxyRequest(proxyPort, "/assets/index-z9.css"),
      ]).then((responses) => responses.map(({ status }) => status)),
      [200, 200, 200],
    );
    assert.equal((await proxyRequest(proxyPort, "/audio")).status, 404);
    assert.equal((await proxyRequest(proxyPort, "/vnc")).status, 404);
    assert.equal(
      (await proxyRequest(proxyPort, "/assets/index.png")).status,
      404,
    );
    assert.equal(
      (await proxyRequest(proxyPort, "/", { method: "POST" })).status,
      403,
    );
    assert.equal(
      (await proxyRequest(proxyPort, "/", { host: `127.0.0.1:${proxyPort}` }))
        .status,
      403,
    );

    const socket = await openSocket(proxyPort, "/stream");
    assert.equal(socket.protocol, "");
    await openSocket(proxyPort, "/control");
    assert.equal(observedConnections, 1, "control must never be observed");
    assert.deepEqual(
      Buffer.concat(observed),
      wire,
      "observe wire bytes including upgrade head, without HTTP headers",
    );
    assert.equal(await rejectedSocket(proxyPort, "/control", origin), 403);
    assert.equal(await rejectedSocket(proxyPort, "/audio", browserOrigin), 403);

    const httpRequests = seen.filter((request) => request.kind === "http");
    assert(httpRequests.length >= 4);
    for (const request of httpRequests) {
      assert.equal(request.authorization, "Bearer trial-token");
      assert.equal(request.cookie, undefined);
      assert.equal(request.host, `127.0.0.1:${upstreamPort}`);
      assert.equal(request.origin, undefined);
    }
    const socketRequest = seen.find((request) => request.kind === "websocket");
    assert.deepEqual(socketRequest, {
      kind: "websocket",
      path: "/stream",
      authorization: "Bearer trial-token",
      cookie: undefined,
      host: `127.0.0.1:${upstreamPort}`,
      origin,
    });

    const closed = new Promise<void>((resolve) =>
      socket.once("close", () => resolve()),
    );
    proxy.revokeCredential();
    await closed;
    assert.equal((await proxyRequest(proxyPort, "/healthz")).status, 200);
    assert.equal(seen.at(-1)?.authorization, undefined);
  } finally {
    proxy?.revokeCredential();
    proxy?.server.closeAllConnections();
    if (proxy?.server.listening) {
      await new Promise<void>((resolve) =>
        proxy!.server.close(() => resolve()),
      );
    }
    agent?.destroy();
    if (upstream) await close(upstream);
    await rm(directory, { recursive: true, force: true });
  }
});
