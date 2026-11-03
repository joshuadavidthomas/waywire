import { Reader } from "./wire.ts";

/** One horizontal stripe of a striped frame: its own H.264 stream. */
export type Stripe = Readonly<{
  y: number;
  height: number;
  key: boolean;
  data: Uint8Array;
}>;

/** Parse the stripe container the compositor's in-process encoder sends. */
export function parseStripes(data: Uint8Array): Stripe[] | null {
  try {
    const reader = new Reader(data);
    const stripes: Stripe[] = [];
    let remaining = data.byteLength;
    while (remaining > 0) {
      const y = reader.u16();
      const height = reader.u16();
      const flags = reader.u8();
      const length = reader.u32();
      if (flags > 1) return null;
      stripes.push({ y, height, key: flags === 1, data: reader.bytes(length) });
      remaining -= 9 + length;
    }
    reader.finish();
    return stripes;
  } catch {
    return null;
  }
}

/** The WebCodecs codec string an Annex-B access unit's sequence parameter set names. */
export function h264Codec(unit: Uint8Array): string | null {
  for (let index = 0; index + 7 < unit.byteLength; index += 1) {
    if (
      unit[index] === 0 &&
      unit[index + 1] === 0 &&
      unit[index + 2] === 1 &&
      ((unit[index + 3] ?? 0) & 0x1f) === 7
    ) {
      const hex = (value: number | undefined) =>
        (value ?? 0).toString(16).padStart(2, "0").toUpperCase();
      return `avc1.${hex(unit[index + 4])}${hex(unit[index + 5])}${hex(unit[index + 6])}`;
    }
  }
  return null;
}

type Composite = {
  readonly timestamp: number;
  remaining: number;
  /** One stripe covers the whole picture, so its decoded frame is the picture. */
  whole: boolean;
};

type StripeDecoder = {
  readonly decoder: VideoDecoder;
  readonly height: number;
};

/**
 * Decodes each stripe with its own WebCodecs decoder and paints it onto one
 * picture. Once every stripe of a frame has been painted, `onFrame` receives
 * the picture as a VideoFrame for the ordinary playout path.
 */
export class StripeCompositor {
  private readonly decoders = new Map<number, StripeDecoder>();
  private readonly composites: Composite[] = [];
  private canvas: OffscreenCanvas | null = null;
  private context: OffscreenCanvasRenderingContext2D | null = null;

  constructor(
    private readonly onFrame: (frame: VideoFrame) => void,
    private readonly onError: (error: Error) => void,
  ) {}

  get decodeQueueSize(): number {
    let size = 0;
    for (const { decoder } of this.decoders.values()) {
      size = Math.max(size, decoder.decodeQueueSize);
    }
    return size;
  }

  reset(): void {
    for (const { decoder } of this.decoders.values()) {
      if (decoder.state !== "closed") decoder.close();
    }
    this.decoders.clear();
    this.composites.length = 0;
  }

  /** Decode one frame's stripes. False when the data is malformed. */
  decode(
    data: Uint8Array,
    timestamp: number,
    width: number,
    height: number,
  ): boolean {
    const stripes = parseStripes(data);
    if (!stripes) return false;
    if (
      !this.canvas ||
      this.canvas.width !== width ||
      this.canvas.height !== height
    ) {
      this.canvas = new OffscreenCanvas(width, height);
      this.context = this.canvas.getContext("2d");
    }
    const composite: Composite = { timestamp, remaining: 0, whole: false };
    for (const stripe of stripes) {
      const decoder = this.decoderFor(stripe);
      if (!decoder) continue;
      composite.remaining += 1;
      decoder.decode(
        new EncodedVideoChunk({
          type: stripe.key ? "key" : "delta",
          timestamp,
          data: stripe.data,
        }),
      );
    }
    // A stream of one stripe never paints the canvas. The stripe layout only
    // changes with the picture's height, which starts a new canvas anyway.
    composite.whole =
      this.decoders.size === 1 &&
      stripes.length === 1 &&
      stripes[0]?.y === 0 &&
      stripes[0].height === height;
    if (composite.remaining > 0) this.composites.push(composite);
    return true;
  }

  private decoderFor(stripe: Stripe): VideoDecoder | null {
    const existing = this.decoders.get(stripe.y);
    if (
      existing &&
      existing.height === stripe.height &&
      existing.decoder.state === "configured" &&
      !stripe.key
    ) {
      return existing.decoder;
    }
    if (!stripe.key) return null;
    const codec = h264Codec(stripe.data);
    if (!codec) return null;
    if (existing) {
      // A key frame restarts the stripe's stream; keep its decoder when it can.
      if (
        existing.height === stripe.height &&
        existing.decoder.state === "configured"
      ) {
        return existing.decoder;
      }
      if (existing.decoder.state !== "closed") existing.decoder.close();
    }
    const y = stripe.y;
    const decoder = new VideoDecoder({
      output: (frame) => this.paint(y, frame),
      error: (error) => {
        this.decoders.delete(y);
        this.onError(error);
      },
    });
    decoder.configure({ codec, optimizeForLatency: true });
    this.decoders.set(y, { decoder, height: stripe.height });
    return decoder;
  }

  private paint(y: number, frame: VideoFrame): void {
    const timestamp = frame.timestamp;
    const index = this.composites.findIndex(
      (composite) => composite.timestamp === timestamp,
    );
    const composite = this.composites[index];
    if (composite?.whole) {
      this.composites.splice(0, index + 1);
      this.onFrame(frame);
      return;
    }
    this.context?.drawImage(frame, 0, y);
    frame.close();
    if (!composite) return;
    composite.remaining -= 1;
    if (composite.remaining > 0 || !this.canvas) return;
    // Older frames still missing a stripe are superseded: the picture already
    // holds everything they would add.
    this.composites.splice(0, index + 1);
    this.onFrame(new VideoFrame(this.canvas, { timestamp }));
  }
}
