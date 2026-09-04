import { describe, expect, it } from "vitest";

import { sign, verify, type TicketPayload } from "./ticket";

const secret = "test-secret-that-is-long-enough-for-this-suite";
const payload: TicketPayload = {
  sub: "josh",
  sprite: "josh-desktop",
  port: 5900,
  exp: 2_000_000_000,
  nonce: "nonce",
};

describe("desktop tickets", () => {
  it("round trips a signed payload", async () => {
    const ticket = await sign(payload, secret);
    await expect(verify(ticket, secret, 1_900_000_000)).resolves.toEqual(
      payload,
    );
  });

  it("rejects a changed payload", async () => {
    const ticket = await sign(payload, secret);
    const [body, signature] = ticket.split(".");
    const changedBody = `${body?.slice(0, -1)}${body?.endsWith("A") ? "B" : "A"}`;
    await expect(
      verify(`${changedBody}.${signature}`, secret, 1_900_000_000),
    ).resolves.toBeNull();
  });

  it("rejects an expired payload", async () => {
    const ticket = await sign(payload, secret);
    await expect(verify(ticket, secret, payload.exp)).resolves.toBeNull();
  });

  it.each(["", "one-part", "a.b.c", "%%%.%%%"])(
    "rejects malformed ticket %j",
    async (ticket) => {
      await expect(verify(ticket, secret)).resolves.toBeNull();
    },
  );
});
