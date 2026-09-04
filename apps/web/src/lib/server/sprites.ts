import {
  DesktopHealth,
  DesktopStatus,
  type DesktopStatus as DesktopStatusValue,
} from "@sprite-desktop/shared/status";

export function requireEnvironment(platform: App.Platform | undefined): Env {
  if (!platform?.env)
    throw new Error("Cloudflare platform environment is unavailable");
  return platform.env;
}

export async function readDesktopStatus(env: Env): Promise<DesktopStatusValue> {
  let health: DesktopStatusValue["health"] = "unknown";

  try {
    const response = await fetch(
      `https://api.sprites.dev/v1/sprites/${encodeURIComponent(env.SPRITE_NAME)}/check`,
      { headers: { Authorization: `Bearer ${env.SPRITES_TOKEN}` } },
    );

    if (response.ok) {
      const body: unknown = await response.json();
      const parsed = DesktopHealth.safeParse(
        typeof body === "object" && body !== null && "status" in body
          ? body.status
          : undefined,
      );
      if (parsed.success) health = parsed.data;
    } else {
      console.error(
        JSON.stringify({
          message: "Sprite health request failed",
          sprite: env.SPRITE_NAME,
          status: response.status,
        }),
      );
    }
  } catch (error) {
    console.error(
      JSON.stringify({
        message: "Sprite health request failed",
        sprite: env.SPRITE_NAME,
        error: error instanceof Error ? error.message : String(error),
      }),
    );
  }

  return DesktopStatus.parse({
    sprite: env.SPRITE_NAME,
    health,
    checkedAt: new Date().toISOString(),
  });
}
