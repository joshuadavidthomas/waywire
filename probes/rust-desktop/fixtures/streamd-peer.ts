#!/usr/bin/env -S node --import tsx

// Independent protocol-v2 peer. It uses only the attributed wire fixture and
// real process pipes/UDP; it does not import either Rust package.
import { spawn } from "node:child_process";
import { appendFileSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";
import dgram from "node:dgram";

interface GoldenWire {
  releaseAll: string;
  keyframeReadyGeneration4: string;
  frameEventGeneration4Sequence17: string;
  cursorVisibilityTrue: string;
  invalid: Array<{ name: string; hex: string }>;
}

function argument(name: string): string {
  const index = process.argv.indexOf(name);
  if (index < 0 || index + 1 === process.argv.length) {
    throw new Error(`missing ${name}`);
  }
  return process.argv[index + 1]!;
}

const fixtureDirectory = dirname(fileURLToPath(import.meta.url));
const wire = JSON.parse(
  readFileSync(join(fixtureDirectory, "wire-v2.json"), "utf8"),
) as GoldenWire;
const port = Number(argument("--rtp-port"));
if (!Number.isInteger(port) || port < 1 || port > 65_535) {
  throw new Error(`invalid RTP port ${port}`);
}

const mode = process.env.SPRITE_GATEWAY_FIXTURE_MODE ?? "live";
const commandLog = process.env.SPRITE_GATEWAY_COMMAND_LOG;
const stateLog = process.env.SPRITE_GATEWAY_STATE_LOG;
const peerPidFile = process.env.SPRITE_GATEWAY_PEER_PID;
const descendantPidFile = process.env.SPRITE_GATEWAY_DESCENDANT_PID;

if (peerPidFile) writeFileSync(peerPidFile, `${process.pid}\n`);
const descendant = descendantPidFile
  ? spawn("sleep", ["60"], { stdio: "ignore" })
  : undefined;
if (descendantPidFile && descendant?.pid) {
  writeFileSync(descendantPidFile, `${descendant.pid}\n`);
}

function note(value: string): void {
  if (stateLog) appendFileSync(stateLog, `${value}\n`);
}

function invalid(name: string): Buffer {
  const value = wire.invalid.find((candidate) => candidate.name === name);
  if (!value) throw new Error(`wire fixture lacks ${name}`);
  return Buffer.from(value.hex, "hex");
}

let commands = Buffer.alloc(0);
if (mode !== "blocked-events") {
  process.stdin.on("data", (part: Buffer) => {
    commands = Buffer.concat([commands, part]);
    while (commands.length >= 16) {
      const kind = commands[1];
      const payloadLength =
        kind === 7 || kind === 10 ? commands.readUInt32LE(4) : 0;
      const recordLength = 16 + payloadLength;
      if (commands.length < recordLength) break;
      const record = commands.subarray(0, recordLength);
      if (commandLog) appendFileSync(commandLog, `${record.toString("hex")}\n`);
      commands = commands.subarray(recordLength);
    }
  });
}

function writeFragmented(bytes: Buffer): Promise<void> {
  return new Promise((resolve, reject) => {
    let index = 0;
    const writeNext = (): void => {
      if (index === bytes.length) {
        resolve();
        return;
      }
      const accepted = process.stdout.write(bytes.subarray(index, index + 1));
      index += 1;
      if (accepted) setImmediate(writeNext);
      else process.stdout.once("drain", writeNext).once("error", reject);
    };
    writeNext();
  });
}

function writeBuffered(bytes: Buffer): Promise<void> {
  return new Promise((resolve) => {
    if (process.stdout.write(bytes)) resolve();
    else process.stdout.once("drain", resolve);
  });
}

const clipboardPayload = Buffer.alloc(1_000_000, "x");
const clipboardEvent = Buffer.alloc(8 + clipboardPayload.length);
clipboardEvent.set([2, 1]);
clipboardEvent.writeUInt32LE(clipboardPayload.length, 4);
clipboardPayload.copy(clipboardEvent, 8);

async function sendClipboardEvents(
  count: number,
  complete: string,
): Promise<void> {
  for (let index = 0; index < count; index += 1) {
    await writeBuffered(clipboardEvent);
  }
  note(complete);
}

function sendKeyframe(): void {
  const packet = Buffer.alloc(14);
  packet.set([0x80, 0xe0]); // version 2, payload 96, marker set
  packet.writeUInt16BE(1, 2);
  packet.writeUInt32BE(3_000, 4);
  packet.writeUInt32BE(7, 8);
  packet.set([0x65, 0x88], 12); // one IDR NAL; marker completes the access unit
  const socket = dgram.createSocket("udp4");
  socket.send(packet, port, "127.0.0.1", (error) => {
    socket.close();
    if (error) throw error;
    note("rtp-marker");
  });
}

async function startLive(): Promise<void> {
  const metadataDelay = Number(
    process.env.SPRITE_GATEWAY_FIXTURE_METADATA_DELAY_MS ?? 100,
  );
  const rtpDelay = Number(
    process.env.SPRITE_GATEWAY_FIXTURE_RTP_DELAY_MS ?? 500,
  );
  await new Promise((resolve) => setTimeout(resolve, metadataDelay));
  await writeFragmented(
    Buffer.from(wire.frameEventGeneration4Sequence17, "hex"),
  );
  note("metadata");
  await new Promise((resolve) => setTimeout(resolve, rtpDelay));
  sendKeyframe();
}

switch (mode) {
  case "live":
  case "blocked-events":
  case "stalled-output":
    void startLive();
    if (mode === "blocked-events") {
      // The parent first proves a large command was accepted into the blocked
      // stdin writer. Flooding 128 cursor events before that point instead
      // tested the gateway's 64-event subscriber limit and closed test sockets.
      process.once("SIGUSR2", () => {
        let writable = true;
        const visibility = Buffer.from(wire.cursorVisibilityTrue, "hex");
        setInterval(() => {
          if (!writable) return;
          writable = process.stdout.write(visibility);
          note("event-batch");
          if (!writable) process.stdout.once("drain", () => (writable = true));
        }, 10);
      });
    } else if (mode === "stalled-output") {
      process.once("SIGUSR2", () => {
        void sendClipboardEvents(10, "stalled-ready");
      });
      process.once("SIGUSR1", () => {
        void sendClipboardEvents(24, "overflow-ready");
      });
    }
    break;
  case "partial-event":
    process.stdout.write(invalid("truncated-event").subarray(0, 2));
    note("partial-event");
    break;
  case "truncated-event":
    process.stdout.end(invalid("truncated-event"));
    note("truncated-event");
    break;
  case "malformed-event":
    process.stdout.write(invalid("malformed-reserved-event"));
    note("malformed-event");
    break;
  case "oversized-event":
    process.stdout.write(invalid("oversized-event"));
    note("oversized-event");
    break;
  case "leader-exit":
    note("leader-exit");
    setTimeout(() => process.exit(23), 100);
    break;
  default:
    throw new Error(`unknown fixture mode ${mode}`);
}

const stop = (): never => process.exit(0);
process.on("SIGTERM", stop);
process.on("SIGINT", stop);
setInterval(() => {}, 10_000);
