import { PROTOCOL_VERSION } from "./messages.ts";

const headerBytes = 8;

export class Writer {
  private buffer = new Uint8Array(64);
  private view = new DataView(this.buffer.buffer);
  private offset = 0;

  private reserve(bytes: number): void {
    const required = this.offset + bytes;
    if (required <= this.buffer.byteLength) return;
    let capacity = this.buffer.byteLength;
    while (capacity < required) capacity *= 2;
    const grown = new Uint8Array(capacity);
    grown.set(this.buffer);
    this.buffer = grown;
    this.view = new DataView(grown.buffer);
  }

  u8(value: number): void {
    this.reserve(1);
    this.view.setUint8(this.offset, value);
    this.offset += 1;
  }

  u16(value: number): void {
    this.reserve(2);
    this.view.setUint16(this.offset, value, true);
    this.offset += 2;
  }

  u32(value: number): void {
    this.reserve(4);
    this.view.setUint32(this.offset, value, true);
    this.offset += 4;
  }

  i32(value: number): void {
    this.reserve(4);
    this.view.setInt32(this.offset, value, true);
    this.offset += 4;
  }

  u64(value: bigint): void {
    this.reserve(8);
    this.view.setBigUint64(this.offset, value, true);
    this.offset += 8;
  }

  f32(value: number): void {
    this.reserve(4);
    this.view.setFloat32(this.offset, value, true);
    this.offset += 4;
  }

  bytes(value: Uint8Array): void {
    this.reserve(value.byteLength);
    this.buffer.set(value, this.offset);
    this.offset += value.byteLength;
  }

  arrayBuffer(): ArrayBuffer {
    const result = new Uint8Array(this.offset);
    result.set(this.buffer.subarray(0, this.offset));
    return result.buffer;
  }
}

export class Reader {
  private readonly data: Uint8Array;
  private readonly view: DataView;
  private offset = 0;

  constructor(buffer: ArrayBuffer | Uint8Array) {
    this.data = buffer instanceof Uint8Array ? buffer : new Uint8Array(buffer);
    this.view = new DataView(
      this.data.buffer,
      this.data.byteOffset,
      this.data.byteLength,
    );
  }

  private require(bytes: number): void {
    if (
      !Number.isSafeInteger(bytes) ||
      bytes < 0 ||
      this.offset + bytes > this.data.byteLength
    ) {
      throw new RangeError("record payload is truncated");
    }
  }

  u8(): number {
    this.require(1);
    const value = this.view.getUint8(this.offset);
    this.offset += 1;
    return value;
  }

  u16(): number {
    this.require(2);
    const value = this.view.getUint16(this.offset, true);
    this.offset += 2;
    return value;
  }

  u32(): number {
    this.require(4);
    const value = this.view.getUint32(this.offset, true);
    this.offset += 4;
    return value;
  }

  i32(): number {
    this.require(4);
    const value = this.view.getInt32(this.offset, true);
    this.offset += 4;
    return value;
  }

  u64(): bigint {
    this.require(8);
    const value = this.view.getBigUint64(this.offset, true);
    this.offset += 8;
    return value;
  }

  f32(): number {
    this.require(4);
    const value = this.view.getFloat32(this.offset, true);
    this.offset += 4;
    return value;
  }

  bytes(length: number): Uint8Array {
    this.require(length);
    const value = this.data.subarray(this.offset, this.offset + length);
    this.offset += length;
    return value;
  }

  rest(): Uint8Array {
    const value = this.data.subarray(this.offset);
    this.offset = this.data.byteLength;
    return value;
  }

  finish(): void {
    if (this.offset !== this.data.byteLength) {
      throw new RangeError("record payload has trailing bytes");
    }
  }
}

export function encodeRecord(
  kind: number,
  writePayload: (writer: Writer) => void,
): ArrayBuffer {
  const payload = new Writer();
  writePayload(payload);
  const payloadBytes = new Uint8Array(payload.arrayBuffer());
  const record = new Writer();
  record.u8(PROTOCOL_VERSION);
  record.u8(kind);
  record.u16(0);
  record.u32(payloadBytes.byteLength);
  record.bytes(payloadBytes);
  return record.arrayBuffer();
}

export function decodeRecord(buffer: ArrayBuffer): {
  readonly kind: number;
  readonly payload: Reader;
} {
  if (!(buffer instanceof ArrayBuffer) || buffer.byteLength < headerBytes) {
    throw new RangeError("record header is truncated");
  }
  const record = new Reader(buffer);
  const version = record.u8();
  const kind = record.u8();
  const reserved = record.u16();
  if (version !== PROTOCOL_VERSION || reserved !== 0) {
    throw new RangeError("record header is invalid");
  }
  const payloadLength = record.u32();
  const payload = record.rest();
  if (payload.byteLength !== payloadLength) {
    throw new RangeError("record length does not match its header");
  }
  return { kind, payload: new Reader(payload) };
}

export function pointerAbsolute(
  x: number,
  y: number,
  sequence: number,
): ArrayBuffer {
  return encodeRecord(1, (payload) => {
    payload.u32(x);
    payload.u32(y);
    payload.u32(sequence);
  });
}

export type Button = 0x110 | 0x111 | 0x112 | 0x113 | 0x114;
export type ButtonState = 0 | 1;
export type KeyState = 0 | 1 | 2;

export function pointerButton(
  button: Button,
  state: ButtonState,
  sequence: number,
): ArrayBuffer {
  return encodeRecord(2, (payload) => {
    payload.u32(button);
    payload.u8(state);
    payload.u32(sequence);
  });
}

export function pointerScroll(
  dx: number,
  dy: number,
  sequence: number,
): ArrayBuffer {
  return encodeRecord(3, (payload) => {
    payload.f32(dx);
    payload.f32(dy);
    payload.u32(sequence);
  });
}

export function keyboardKey(
  key: number,
  state: KeyState,
  sequence: number,
): ArrayBuffer {
  return encodeRecord(4, (payload) => {
    payload.u32(key);
    payload.u8(state);
    payload.u32(sequence);
  });
}

export function releaseAll(): ArrayBuffer {
  return encodeRecord(5, () => undefined);
}

export function resize(
  width: number,
  height: number,
  scaleV120: number,
  requestId: number,
): ArrayBuffer {
  return encodeRecord(6, (payload) => {
    payload.u32(width);
    payload.u32(height);
    payload.u16(scaleV120);
    payload.u16(requestId);
  });
}

export function pointerRelative(
  dx: number,
  dy: number,
  sequence: number,
): ArrayBuffer {
  return encodeRecord(8, (payload) => {
    payload.f32(dx);
    payload.f32(dy);
    payload.u32(sequence);
  });
}

export type FrameMetadata = Readonly<{
  generation: number;
  width: number;
  height: number;
  captureNanos: bigint;
  sequence: bigint;
  inputSequence: number;
  fps: number;
}>;

export function readFrameMetadata(reader: Reader): FrameMetadata {
  return {
    generation: reader.u32(),
    width: reader.u16(),
    height: reader.u16(),
    captureNanos: reader.u64(),
    sequence: reader.u64(),
    inputSequence: reader.u32(),
    fps: reader.u32(),
  };
}

export type FrameKind = 0 | 1;
export type Continuity = 0 | 1;

export function videoFrame(
  frameKind: FrameKind,
  continuity: Continuity,
  metadata: FrameMetadata,
  data: Uint8Array,
): ArrayBuffer {
  return encodeRecord(1, (payload) => {
    payload.u8(frameKind);
    payload.u8(continuity);
    payload.u32(metadata.generation);
    payload.u16(metadata.width);
    payload.u16(metadata.height);
    payload.u64(metadata.captureNanos);
    payload.u64(metadata.sequence);
    payload.u32(metadata.inputSequence);
    payload.u32(metadata.fps);
    payload.bytes(data);
  });
}
