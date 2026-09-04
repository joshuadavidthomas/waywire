import { performance } from "node:perf_hooks";

/**
 * Header-only timing metadata observed from one uncompressed RFC 6455 server
 * stream. The observer never retains WebSocket payload beyond the fixed
 * 40-byte video header. It accepts no masking or negotiated extensions.
 */

const MAX_VIDEO_MESSAGE_BYTES = 16 * 1024 * 1024 + 40;
const MAX_RECORDS = 10_000;
const VIDEO_HEADER_BYTES = 40;
const MALFORMED_ERROR = "malformed-websocket-server-stream" as const;

type MessageKind = "binary" | "text";

export interface ProxyVideoTimingRecord {
  readonly generation: number;
  readonly sequence: string;
  readonly mediaTimestampMicros: string;
  readonly captureNanos: string;
  readonly flags: number;
  readonly auBytes: number;
  readonly firstByteAtMs: number;
  readonly completedAtMs: number;
}

export interface ProxyVideoTimingsSnapshot {
  readonly performanceStartMs: number;
  readonly records: readonly ProxyVideoTimingRecord[];
  readonly overflow: number;
  readonly error: typeof MALFORMED_ERROR | null;
}

interface MessageCapture {
  readonly header: Uint8Array;
  readonly firstByteAtMs: number;
  headerBytes: number;
}

/**
 * Pass every upstream `/stream` byte to this observer, including the upgrade
 * head and bytes received outside a sample window. `receive` treats bad wire
 * data as a terminal observation error and never throws into the product pipe.
 * `firstByteAtMs` marks the first byte of the message's first WebSocket frame;
 * `completedAtMs` marks consumption of the final payload byte.
 *
 * A call to `start` replaces the prior sample. If it resets an active sample
 * while a binary message is in flight, that message may finish in the new
 * sample and have a negative `firstByteAtMs`. Messages whose WebSocket header
 * starts while sampling is disabled are skipped, even if they finish later.
 */
export class ProxyVideoTimings {
  readonly #now: () => number;
  readonly #webSocketHeader = new Uint8Array(10);

  #headerBytes = 0;
  #headerNeeded = 2;
  #frameFirstByteAtMs: number | undefined;
  #frameRemaining: number | null = null;
  #frameOpcode = 0;
  #frameFin = false;

  #messageKind: MessageKind | null = null;
  #messageBytes = 0;
  #messageCapture: MessageCapture | null = null;

  #sampling = false;
  #performanceStartMs: number | null = null;
  #records: ProxyVideoTimingRecord[] = [];
  #overflow = 0;
  #error: typeof MALFORMED_ERROR | null = null;

  constructor(now: () => number = () => performance.now()) {
    this.#now = now;
  }

  start(): void {
    this.#records = [];
    this.#overflow = 0;
    this.#sampling = true;
    this.#performanceStartMs = this.#now();
  }

  finish(): ProxyVideoTimingsSnapshot {
    if (!this.#sampling || this.#performanceStartMs === null) {
      throw new Error("Proxy video timing observation was not started");
    }
    const performanceStartMs = this.#performanceStartMs;
    const snapshot: ProxyVideoTimingsSnapshot = {
      performanceStartMs,
      records: this.#records,
      overflow: this.#overflow,
      error: this.#error,
    };
    this.#sampling = false;
    this.#performanceStartMs = null;
    this.#messageCapture = null;
    return snapshot;
  }

  receive(chunk: Uint8Array): void {
    if (this.#error !== null) return;
    const receivedAtMs = this.#sampling ? this.#now() : undefined;
    let offset = 0;

    while (offset < chunk.byteLength && this.#error === null) {
      if (this.#frameRemaining === null) {
        if (this.#headerBytes === 0) {
          this.#frameFirstByteAtMs = receivedAtMs;
        }
        const copied = Math.min(
          this.#headerNeeded - this.#headerBytes,
          chunk.byteLength - offset,
        );
        this.#webSocketHeader.set(
          chunk.subarray(offset, offset + copied),
          this.#headerBytes,
        );
        this.#headerBytes += copied;
        offset += copied;

        if (this.#headerBytes >= 2 && this.#headerNeeded === 2) {
          const lengthCode = this.#webSocketHeader[1]! & 0x7f;
          this.#headerNeeded =
            lengthCode === 126 ? 4 : lengthCode === 127 ? 10 : 2;
        }
        if (this.#headerBytes !== this.#headerNeeded) continue;
        this.#beginFrame();
        if (this.#error !== null) return;
        if (this.#frameRemaining === 0) this.#completeFrame(receivedAtMs);
        continue;
      }

      const consumed = Math.min(
        this.#frameRemaining,
        chunk.byteLength - offset,
      );
      if (consumed > 0 && this.#frameOpcode < 8) {
        this.#consumeMessageBytes(chunk, offset, consumed);
      }
      offset += consumed;
      this.#frameRemaining -= consumed;
      if (this.#frameRemaining === 0) this.#completeFrame(receivedAtMs);
    }
  }

  #beginFrame(): void {
    const first = this.#webSocketHeader[0]!;
    const second = this.#webSocketHeader[1]!;
    const fin = (first & 0x80) !== 0;
    const opcode = first & 0x0f;
    const masked = (second & 0x80) !== 0;
    const lengthCode = second & 0x7f;

    if (
      (first & 0x70) !== 0 ||
      masked ||
      ![0, 1, 2, 8, 9, 10].includes(opcode)
    ) {
      this.#fail();
      return;
    }

    let payloadBytes: number;
    if (lengthCode < 126) {
      payloadBytes = lengthCode;
    } else if (lengthCode === 126) {
      payloadBytes =
        this.#webSocketHeader[2]! * 0x100 + this.#webSocketHeader[3]!;
      if (payloadBytes < 126) {
        this.#fail();
        return;
      }
    } else {
      let value = 0n;
      for (let index = 2; index < 10; index += 1) {
        value = (value << 8n) | BigInt(this.#webSocketHeader[index]!);
      }
      if (value < 65_536n || value > BigInt(MAX_VIDEO_MESSAGE_BYTES)) {
        this.#fail();
        return;
      }
      payloadBytes = Number(value);
    }

    if (opcode >= 8) {
      if (!fin || payloadBytes > 125 || (opcode === 8 && payloadBytes === 1)) {
        this.#fail();
        return;
      }
    } else if (opcode === 0) {
      if (this.#messageKind === null) {
        this.#fail();
        return;
      }
    } else {
      if (this.#messageKind !== null) {
        this.#fail();
        return;
      }
      this.#messageKind = opcode === 2 ? "binary" : "text";
      this.#messageBytes = 0;
      this.#messageCapture =
        opcode === 2 && this.#sampling && this.#frameFirstByteAtMs !== undefined
          ? {
              header: new Uint8Array(VIDEO_HEADER_BYTES),
              firstByteAtMs: this.#frameFirstByteAtMs,
              headerBytes: 0,
            }
          : null;
    }

    if (
      opcode < 8 &&
      this.#messageBytes + payloadBytes > MAX_VIDEO_MESSAGE_BYTES
    ) {
      this.#fail();
      return;
    }

    this.#frameOpcode = opcode;
    this.#frameFin = fin;
    this.#frameRemaining = payloadBytes;
  }

  #consumeMessageBytes(
    chunk: Uint8Array,
    offset: number,
    length: number,
  ): void {
    const capture = this.#messageCapture;
    if (capture !== null && capture.headerBytes < VIDEO_HEADER_BYTES) {
      const copied = Math.min(VIDEO_HEADER_BYTES - capture.headerBytes, length);
      capture.header.set(
        chunk.subarray(offset, offset + copied),
        capture.headerBytes,
      );
      capture.headerBytes += copied;
    }
    this.#messageBytes += length;
  }

  #completeFrame(receivedAtMs: number | undefined): void {
    const completesMessage = this.#frameOpcode < 8 && this.#frameFin;
    if (completesMessage) this.#completeMessage(receivedAtMs);

    this.#frameRemaining = null;
    this.#frameOpcode = 0;
    this.#frameFin = false;
    this.#frameFirstByteAtMs = undefined;
    this.#headerBytes = 0;
    this.#headerNeeded = 2;
  }

  #completeMessage(receivedAtMs: number | undefined): void {
    const capture = this.#messageCapture;
    if (
      this.#messageKind === "binary" &&
      capture !== null &&
      capture.headerBytes === VIDEO_HEADER_BYTES &&
      this.#sampling &&
      this.#performanceStartMs !== null &&
      receivedAtMs !== undefined
    ) {
      const header = new DataView(
        capture.header.buffer,
        capture.header.byteOffset,
        capture.header.byteLength,
      );
      if (header.getUint8(0) === 2 && header.getUint8(1) === 1) {
        const record: ProxyVideoTimingRecord = {
          generation: header.getUint32(20, true),
          sequence: header.getBigUint64(4, true).toString(),
          mediaTimestampMicros: header.getBigUint64(12, true).toString(),
          captureNanos: header.getBigUint64(28, true).toString(),
          flags: header.getUint8(2),
          auBytes: this.#messageBytes - VIDEO_HEADER_BYTES,
          firstByteAtMs: capture.firstByteAtMs - this.#performanceStartMs,
          completedAtMs: receivedAtMs - this.#performanceStartMs,
        };
        if (this.#records.length < MAX_RECORDS) this.#records.push(record);
        else if (this.#overflow < Number.MAX_SAFE_INTEGER) this.#overflow += 1;
      }
    }

    this.#messageKind = null;
    this.#messageBytes = 0;
    this.#messageCapture = null;
  }

  #fail(): void {
    this.#error = MALFORMED_ERROR;
    this.#messageCapture = null;
  }
}
