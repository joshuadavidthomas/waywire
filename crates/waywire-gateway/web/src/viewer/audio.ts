// Desktop audio. The gateway's /audio socket carries one 20 ms Opus packet
// per binary message; WebCodecs decodes them and an AudioContext plays them
// back to back. Browsers let a page start audio only from a user gesture, so
// the viewer calls start() from its first click or key. Audio has its own
// socket and never touches the video path.

const sampleRate = 48_000;
const channels = 2;
const packetMicros = 20_000;
/** Lead given to the first chunk and to the first chunk after a gap. */
const startLeadSeconds = 0.04;
/** A chunk due this soon has missed its slot. */
const minimumLeadSeconds = 0.005;
/** A chunk due later than this is dropped, so a burst after a stall does not
 * become lasting delay. */
const maximumLeadSeconds = 0.12;
const reconnectMs = 1000;

/**
 * When to play a chunk that would follow the previous one at `next`, with the
 * audio clock at `now`; `null` drops it.
 */
export function chunkStart(next: number, now: number): number | null {
  if (next < now + minimumLeadSeconds) return now + startLeadSeconds;
  if (next > now + maximumLeadSeconds) return null;
  return next;
}

export class DesktopAudio {
  private context: AudioContext | null = null;
  private socket: WebSocket | null = null;
  private reconnect: ReturnType<typeof setTimeout> | undefined;
  private next = 0;
  private disposed = false;

  constructor(private readonly url: URL) {}

  /**
   * Start playback from a user gesture. Resolves to whether audio is playing,
   * after which later gestures need not call again.
   */
  async start(): Promise<boolean> {
    if (this.disposed || typeof AudioDecoder === "undefined") return false;
    if (!this.context) {
      this.context = new AudioContext({
        sampleRate,
        latencyHint: "interactive",
      });
      this.connect();
    }
    await this.context.resume();
    return this.context.state === "running";
  }

  dispose(): void {
    this.disposed = true;
    clearTimeout(this.reconnect);
    this.socket?.close();
    void this.context?.close();
  }

  private connect(): void {
    const socket = new WebSocket(this.url);
    socket.binaryType = "arraybuffer";
    this.socket = socket;
    const decoder = new AudioDecoder({
      output: (data) => this.play(data),
      error: (error) => {
        console.warn("desktop audio decoder failed", error);
        socket.close();
      },
    });
    decoder.configure({
      codec: "opus",
      sampleRate,
      numberOfChannels: channels,
    });
    let timestamp = 0;
    socket.addEventListener("message", (event: MessageEvent<unknown>) => {
      if (!(event.data instanceof ArrayBuffer)) return;
      if (decoder.state !== "configured") return;
      decoder.decode(
        new EncodedAudioChunk({ type: "key", timestamp, data: event.data }),
      );
      timestamp += packetMicros;
    });
    socket.addEventListener("close", () => {
      if (decoder.state !== "closed") decoder.close();
      if (this.disposed) return;
      this.reconnect = setTimeout(() => this.connect(), reconnectMs);
    });
  }

  private play(data: AudioData): void {
    const context = this.context;
    if (!context || this.disposed) {
      data.close();
      return;
    }
    const buffer = context.createBuffer(
      data.numberOfChannels,
      data.numberOfFrames,
      data.sampleRate,
    );
    for (let channel = 0; channel < data.numberOfChannels; channel += 1) {
      const samples = new Float32Array(data.numberOfFrames);
      data.copyTo(samples, { planeIndex: channel, format: "f32-planar" });
      buffer.copyToChannel(samples, channel);
    }
    data.close();
    const at = chunkStart(this.next, context.currentTime);
    if (at === null) return;
    const source = context.createBufferSource();
    source.buffer = buffer;
    source.connect(context.destination);
    source.start(at);
    this.next = at + buffer.duration;
  }
}
