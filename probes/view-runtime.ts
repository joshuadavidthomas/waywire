// Browser acceptance helpers, not product transports. The HTTPS mode keeps the
// org token in this process. The tunnel experiment connects only to one
// localhost port and sends canonical Sprite identity headers without credentials.
import {
  createServer,
  request as requestHTTP,
  type ClientRequest,
  type IncomingMessage,
  type OutgoingHttpHeaders,
  type RequestOptions,
} from "node:http";
import { request as requestHTTPS, type Agent } from "node:https";
import type { Duplex } from "node:stream";

export type AcceptanceProfile = "vnc" | "waymote" | "rust";

const profiles: Record<
  AcceptanceProfile,
  { readonly http: RegExp; readonly sockets: readonly string[] }
> = {
  vnc: {
    http: /^\/(?:assets\/[^/?]+|healthz|version)?(?:\?.*)?$/u,
    sockets: ["/vnc"],
  },
  waymote: {
    http: /^\/(?:client\.js|style\.css|waymote\.js|audio-player\.js|recorder\.mjs|metrics\.mjs|healthz)?(?:\?.*)?$/u,
    sockets: ["/stream", "/control", "/audio"],
  },
  rust: {
    http: /^\/(?:assets\/[A-Za-z0-9_-]+\.(?:css|js)|healthz)?(?:\?.*)?$/u,
    sockets: ["/stream", "/control"],
  },
};

type UpstreamRequest = {
  (url: URL, options: RequestOptions): ClientRequest;
  (
    url: URL,
    options: RequestOptions,
    callback: (response: IncomingMessage) => void,
  ): ClientRequest;
};

type ForwardingTarget = {
  readonly origin: URL;
  readonly request: UpstreamRequest;
  readonly agent?: Agent;
  readonly httpHeaders: () => OutgoingHttpHeaders;
  readonly websocketHeaders: () => OutgoingHttpHeaders;
  readonly revoke: () => void;
};

type RustVideoObserver = (upstream: Duplex) => (chunk: Buffer) => void;

function parseCanonicalHttpsOrigin(origin: string, message: string): URL {
  const parsed = new URL(origin);
  if (parsed.origin !== origin || parsed.protocol !== "https:")
    throw new Error(message);
  return parsed;
}

function createForwardingProxy(
  profile: AcceptanceProfile,
  target: ForwardingTarget,
  observeRustVideo?: RustVideoObserver,
) {
  if (observeRustVideo && profile !== "rust")
    throw new Error("Video timing observation requires the Rust profile");
  const sockets = new Set<Duplex>();
  const local = "http://127.0.0.1:3217";
  const { http: httpPaths, sockets: socketPaths } = profiles[profile];
  const server = createServer((req, res) => {
    if (
      req.headers.host !== "127.0.0.1:3217" ||
      !["GET", "HEAD"].includes(req.method!)
    ) {
      res.writeHead(403).end();
      return;
    }
    if (!httpPaths.test(req.url ?? "")) {
      res.writeHead(404).end();
      return;
    }
    const upstream = target.request(
      new URL(req.url!, target.origin),
      {
        agent: target.agent,
        method: req.method,
        headers: target.httpHeaders(),
      },
      (response) => {
        const headers = { ...response.headers };
        delete headers["set-cookie"];
        res.writeHead(response.statusCode!, headers);
        response.pipe(res);
      },
    );
    upstream.on("error", () => {
      if (!res.headersSent) res.writeHead(502);
      res.end();
    });
    res.on("close", () => upstream.destroy());
    upstream.end();
  });
  server.on("upgrade", (req, client, head) => {
    if (
      !socketPaths.includes(req.url ?? "") ||
      req.headers.host !== "127.0.0.1:3217" ||
      req.headers.origin !== local
    ) {
      client.end("HTTP/1.1 403 Forbidden\r\nConnection: close\r\n\r\n");
      return;
    }
    sockets.add(client);
    client.on("close", () => sockets.delete(client));
    const upstream = target.request(new URL(req.url!, target.origin), {
      agent: target.agent,
      headers: {
        Connection: "Upgrade",
        Upgrade: "websocket",
        "Sec-WebSocket-Key": req.headers["sec-websocket-key"]!,
        "Sec-WebSocket-Version": "13",
        ...target.websocketHeaders(),
      },
    });
    upstream.on("upgrade", (response, socket, upstreamHead) => {
      if (req.url === "/stream" && observeRustVideo) {
        const observe = observeRustVideo(socket);
        // Register before pipe's data listener; this marks arrival before local
        // forwarding. Include bytes delivered alongside the upgrade response.
        if (upstreamHead.length) observe(upstreamHead);
        socket.on("data", observe);
      }
      client.write(
        `HTTP/1.1 101 Switching Protocols\r\n${Object.entries(response.headers)
          .filter(([key]) => key.toLowerCase() !== "set-cookie")
          .map(([key, value]) => `${key}: ${value}`)
          .join("\r\n")}\r\n\r\n`,
      );
      if (head.length) socket.write(head);
      if (upstreamHead.length) client.write(upstreamHead);
      socket.pipe(client).pipe(socket);
      socket.on("error", () => client.destroy());
      client.on("error", () => socket.destroy());
      socket.on("close", () => client.destroy());
      client.on("close", () => socket.destroy());
    });
    upstream.on("response", (response) => {
      response.resume();
      client.end(
        `HTTP/1.1 ${response.statusCode} Upstream refused\r\nConnection: close\r\n\r\n`,
      );
    });
    upstream.on("error", () => client.destroy());
    client.on("close", () => upstream.destroy());
    upstream.end();
  });
  return {
    server,
    revokeCredential() {
      target.revoke();
      for (const socket of sockets) socket.destroy();
    },
  };
}

export function createAcceptanceProxy(
  origin: string,
  token: string,
  profile: AcceptanceProfile = "vnc",
  httpsAgent?: Agent,
  // One passive, non-throwing byte observer per Rust video connection.
  observeRustVideo?: RustVideoObserver,
) {
  if (observeRustVideo && profile !== "rust")
    throw new Error("Video timing observation requires the Rust profile");
  const validationError = "A canonical HTTPS origin and token are required";
  const upstreamOrigin = parseCanonicalHttpsOrigin(origin, validationError);
  if (!token) throw new Error(validationError);
  let credential: string | undefined = token;
  const authorization = () =>
    credential ? { Authorization: `Bearer ${credential}` } : {};
  return createForwardingProxy(
    profile,
    {
      origin: upstreamOrigin,
      request: requestHTTPS,
      agent: httpsAgent,
      httpHeaders: authorization,
      websocketHeaders: () => ({
        host: upstreamOrigin.host,
        origin,
        ...authorization(),
      }),
      revoke: () => {
        credential = undefined;
      },
    },
    observeRustVideo,
  );
}

// Probe-only experiment: the transport target cannot escape loopback, while
// gateway validation still sees the canonical Sprite Host and Origin.
export function createRustTunnelAcceptanceProxy(
  canonicalHttpsOrigin: string,
  tunnelPort: number,
  // One passive, non-throwing byte observer per Rust video connection.
  observeRustVideo?: RustVideoObserver,
) {
  const spriteOrigin = parseCanonicalHttpsOrigin(
    canonicalHttpsOrigin,
    "A canonical HTTPS origin is required",
  );
  if (!Number.isInteger(tunnelPort) || tunnelPort < 1 || tunnelPort > 65_535)
    throw new Error("A valid localhost tunnel port is required");
  const gatewayHeaders = () => ({
    host: spriteOrigin.host,
    origin: canonicalHttpsOrigin,
  });
  return createForwardingProxy(
    "rust",
    {
      origin: new URL(`http://127.0.0.1:${tunnelPort}`),
      request: requestHTTP,
      httpHeaders: gatewayHeaders,
      websocketHeaders: gatewayHeaders,
      revoke: () => {},
    },
    observeRustVideo,
  );
}

if (import.meta.main) {
  const { SPRITE_DESKTOP_URL, SPRITES_TOKEN } = process.env;
  if (!SPRITE_DESKTOP_URL || !SPRITES_TOKEN)
    throw new Error("SPRITE_DESKTOP_URL and SPRITES_TOKEN are required");
  const { server } = createAcceptanceProxy(SPRITE_DESKTOP_URL, SPRITES_TOKEN);
  server.listen(3217, "127.0.0.1", () =>
    console.log(
      `Acceptance browser URL: http://127.0.0.1:3217; process ${process.pid}`,
    ),
  );
}
