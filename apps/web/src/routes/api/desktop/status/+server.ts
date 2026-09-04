import { json } from "@sveltejs/kit";

import { readDesktopStatus, requireEnvironment } from "$lib/server/sprites";

import type { RequestHandler } from "./$types";

export const GET: RequestHandler = async ({ platform }) => {
  const status = await readDesktopStatus(requireEnvironment(platform));
  return json(status, { headers: { "Cache-Control": "no-store" } });
};
