import { z } from "zod";

const encoder = new TextEncoder();

export const TicketPayload = z.object({
  sub: z.string().min(1),
  sprite: z.string().min(1),
  port: z.number().int().min(1).max(65_535),
  exp: z.number().int().positive(),
  nonce: z.string().min(1),
});

export const DesktopTicket = z.object({
  ticket: z.string().min(1),
  wsUrl: z.url(),
});

export type TicketPayload = z.infer<typeof TicketPayload>;
export type DesktopTicket = z.infer<typeof DesktopTicket>;

function encodeBase64Url(bytes: Uint8Array): string {
  let binary = "";
  for (const byte of bytes) binary += String.fromCharCode(byte);
  return btoa(binary)
    .replaceAll("+", "-")
    .replaceAll("/", "_")
    .replace(/=+$/u, "");
}

function decodeBase64Url(value: string): Uint8Array<ArrayBuffer> | null {
  if (!/^[A-Za-z0-9_-]+$/u.test(value)) return null;

  const padding = "=".repeat((4 - (value.length % 4)) % 4);
  try {
    const binary = atob(
      value.replaceAll("-", "+").replaceAll("_", "/") + padding,
    );
    const bytes = new Uint8Array(binary.length);
    for (let index = 0; index < binary.length; index += 1) {
      bytes[index] = binary.charCodeAt(index);
    }
    return bytes;
  } catch {
    return null;
  }
}

async function importKey(secret: string): Promise<CryptoKey> {
  return crypto.subtle.importKey(
    "raw",
    encoder.encode(secret),
    { name: "HMAC", hash: "SHA-256" },
    false,
    ["sign", "verify"],
  );
}

export async function sign(
  payload: TicketPayload,
  secret: string,
): Promise<string> {
  const parsed = TicketPayload.parse(payload);
  const body = encodeBase64Url(encoder.encode(JSON.stringify(parsed)));
  const signature = await crypto.subtle.sign(
    "HMAC",
    await importKey(secret),
    encoder.encode(body),
  );
  return `${body}.${encodeBase64Url(new Uint8Array(signature))}`;
}

export async function verify(
  ticket: string,
  secret: string,
  now = Math.floor(Date.now() / 1000),
): Promise<TicketPayload | null> {
  const parts = ticket.split(".");
  if (parts.length !== 2) return null;

  const [body, encodedSignature] = parts;
  if (!body || !encodedSignature) return null;

  const signature = decodeBase64Url(encodedSignature);
  const payloadBytes = decodeBase64Url(body);
  if (!signature || !payloadBytes) return null;

  const validSignature = await crypto.subtle.verify(
    "HMAC",
    await importKey(secret),
    signature,
    encoder.encode(body),
  );
  if (!validSignature) return null;

  try {
    const payload = TicketPayload.parse(
      JSON.parse(new TextDecoder().decode(payloadBytes)),
    );
    return payload.exp > now ? payload : null;
  } catch {
    return null;
  }
}
