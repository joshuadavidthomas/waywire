import { z } from "zod";

export const DesktopHealth = z.enum([
  "healthy",
  "hibernated",
  "unhealthy",
  "error",
  "unknown",
]);

export const DesktopStatus = z.object({
  sprite: z.string().min(1),
  health: DesktopHealth,
  checkedAt: z.iso.datetime(),
});

export type DesktopHealth = z.infer<typeof DesktopHealth>;
export type DesktopStatus = z.infer<typeof DesktopStatus>;
