import { beforeEach, describe, expect, it, vi } from "vitest";

import { sign } from "@sprite-desktop/shared/ticket";

import worker from "./index";

const env = {
  SPRITES_TOKEN: "sprites-token",
  TICKET_SECRET: "ticket-secret",
} satisfies Env;

function request(path: string, headers?: HeadersInit): Request {
  return new Request(`https://gateway.example${path}`, { headers });
}

beforeEach(() => {
  vi.stubGlobal("fetch", () => {
    throw new Error("upstream fetch should not run in rejection tests");
  });
});

describe("gateway request validation", () => {
  it("returns 404 outside the VNC path", async () => {
    const response = await worker.fetch(request("/"), env);
    expect(response.status).toBe(404);
  });

  it("requires a WebSocket upgrade", async () => {
    const response = await worker.fetch(request("/vnc"), env);
    expect(response.status).toBe(426);
  });

  it("rejects a bad ticket", async () => {
    const response = await worker.fetch(
      request("/vnc?ticket=bad", { Upgrade: "websocket" }),
      env,
    );
    expect(response.status).toBe(403);
  });

  it("rejects an expired ticket", async () => {
    const ticket = await sign(
      {
        sub: "josh",
        sprite: "josh-desktop",
        port: 5900,
        exp: 1,
        nonce: "expired",
      },
      env.TICKET_SECRET,
    );
    const response = await worker.fetch(
      request(`/vnc?ticket=${ticket}`, { Upgrade: "websocket" }),
      env,
    );
    expect(response.status).toBe(403);
  });

  it("passes through an authorized upstream WebSocket response", async () => {
    const ticket = await sign(
      {
        sub: "josh",
        sprite: "josh-desktop",
        port: 5900,
        exp: Math.floor(Date.now() / 1000) + 60,
        nonce: "valid",
      },
      env.TICKET_SECRET,
    );
    const upstream = { status: 101, webSocket: {} } as Response;
    const fetchMock = vi.fn(
      async (_input: RequestInfo | URL, _init?: RequestInit) => upstream,
    );
    vi.stubGlobal("fetch", fetchMock);

    const response = await worker.fetch(
      request(`/vnc?ticket=${ticket}`, {
        Upgrade: "websocket",
        "Sec-WebSocket-Protocol": "binary",
      }),
      env,
    );

    expect(response).toBe(upstream);
    expect(fetchMock).toHaveBeenCalledOnce();
    const [url, init] = fetchMock.mock.calls[0] ?? [];
    expect(url).toBe("https://api.sprites.dev/v1/sprites/josh-desktop/proxy");
    const headers = new Headers(init?.headers);
    expect(headers.get("Authorization")).toBe("Bearer sprites-token");
    expect(headers.get("Upgrade")).toBe("websocket");
    expect(headers.get("Sec-WebSocket-Protocol")).toBe("binary");
  });
});
