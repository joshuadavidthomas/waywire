import { verify } from "@sprite-desktop/shared/ticket";

function text(message: string, status: number): Response {
  return new Response(message, {
    status,
    headers: { "Content-Type": "text/plain; charset=utf-8" },
  });
}

async function handleRequest(request: Request, env: Env): Promise<Response> {
  const url = new URL(request.url);
  if (url.pathname !== "/vnc") return text("not found", 404);
  if (request.headers.get("Upgrade")?.toLowerCase() !== "websocket") {
    return text("expected websocket", 426);
  }

  const payload = await verify(
    url.searchParams.get("ticket") ?? "",
    env.TICKET_SECRET,
  );
  if (!payload) return text("bad ticket", 403);

  const headers = new Headers({
    Authorization: `Bearer ${env.SPRITES_TOKEN}`,
    Upgrade: "websocket",
  });
  const protocol = request.headers.get("Sec-WebSocket-Protocol");
  if (protocol) headers.set("Sec-WebSocket-Protocol", protocol);

  const upstream = await fetch(
    `https://api.sprites.dev/v1/sprites/${encodeURIComponent(payload.sprite)}/proxy`,
    { headers },
  );

  if (upstream.status !== 101 || !upstream.webSocket) {
    console.error(
      JSON.stringify({
        message: "Sprites proxy rejected WebSocket upgrade",
        sprite: payload.sprite,
        status: upstream.status,
      }),
    );
    return text(`upstream websocket upgrade failed (${upstream.status})`, 502);
  }

  return upstream;
}

export default {
  async fetch(request: Request, env: Env): Promise<Response> {
    try {
      return await handleRequest(request, env);
    } catch (error) {
      console.error(
        JSON.stringify({
          message: "Gateway request failed",
          error: error instanceof Error ? error.message : String(error),
          path: new URL(request.url).pathname,
        }),
      );
      return text("gateway request failed", 500);
    }
  },
} satisfies ExportedHandler<Env>;
