import { randomBytes } from "node:crypto";
import { writeFile } from "node:fs/promises";
import { resolve } from "node:path";

const token = process.env.SPRITES_TOKEN;
if (!token) throw new Error("SPRITES_TOKEN is required");

const ticketSecret =
  process.env.TICKET_SECRET ?? randomBytes(32).toString("base64url");
const repository = resolve(import.meta.dirname, "..");
const contents = `SPRITES_TOKEN=${token}\nTICKET_SECRET=${ticketSecret}\n`;
const paths = [
  resolve(repository, "apps/web/.dev.vars"),
  resolve(repository, "apps/gateway/.dev.vars"),
];

await Promise.all(
  paths.map((path) => writeFile(path, contents, { mode: 0o600 })),
);
console.log(
  "Wrote matching local secrets to apps/web/.dev.vars and apps/gateway/.dev.vars.",
);
