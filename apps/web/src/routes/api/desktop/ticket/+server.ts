import { sign } from "@sprite-desktop/shared/ticket";
import { json } from "@sveltejs/kit";

import { requireEnvironment } from "$lib/server/sprites";

import type { RequestHandler } from "./$types";

export const GET: RequestHandler = async ({ platform }) => {
  const env = requireEnvironment(platform);
  const ticket = await sign(
    {
      sub: "josh",
      sprite: env.SPRITE_NAME,
      port: 5900,
      exp: Math.floor(Date.now() / 1000) + 60,
      nonce: crypto.randomUUID(),
    },
    env.TICKET_SECRET,
  );

  const wsUrl = new URL(env.GATEWAY_WS_URL);
  wsUrl.searchParams.set("ticket", ticket);

  return json(
    { ticket, wsUrl: wsUrl.toString() },
    { headers: { "Cache-Control": "no-store" } },
  );
};
