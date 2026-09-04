// AudioWorklet companion loaded internally by WaymoteSession.
declare const sampleRate: number;
declare const currentFrame: number;
declare abstract class AudioWorkletProcessor {
  readonly port: MessagePort;
  constructor();
  abstract process(
    inputs: Float32Array[][],
    outputs: Float32Array[][],
  ): boolean;
}
declare function registerProcessor(
  name: string,
  processor: new () => AudioWorkletProcessor,
): void;

type AudioChunk = {
  readonly channels: Float32Array[];
  offset: number;
  readonly startFrame: number | null;
  readonly captureTimestamp: number | null;
};

type PlayerMessage =
  | { readonly type: "reset" }
  | {
      readonly type: "samples";
      readonly channels: Float32Array[];
      readonly startFrame: number | null;
      readonly captureTimestamp: number | null;
    };

function decodePlayerMessage(value: unknown): PlayerMessage | null {
  if (typeof value !== "object" || value === null || !("type" in value))
    return null;
  if (value.type === "reset") return { type: "reset" };
  if (
    value.type !== "samples" ||
    !("channels" in value) ||
    !Array.isArray(value.channels) ||
    value.channels.length === 0 ||
    !value.channels.every((channel) => channel instanceof Float32Array)
  )
    return null;
  const startFrame =
    "startFrame" in value &&
    typeof value.startFrame === "number" &&
    Number.isFinite(value.startFrame)
      ? value.startFrame
      : null;
  const captureTimestamp =
    "captureTimestamp" in value &&
    typeof value.captureTimestamp === "number" &&
    Number.isFinite(value.captureTimestamp)
      ? value.captureTimestamp
      : null;
  return {
    type: "samples",
    channels: value.channels,
    startFrame,
    captureTimestamp,
  };
}

class WaymoteAudioPlayer extends AudioWorkletProcessor {
  private queue: AudioChunk[] = [];
  private queuedFrames = 0;
  private playing = false;
  private underflows = 0;
  private reportCountdown = 0;
  private scheduled: boolean | null = null;
  private lastCaptureTimestamp: number | null = null;
  private lastCaptureFrame: number | null = null;
  private readonly targetFrames = Math.round(sampleRate * 0.06);
  private readonly maximumFrames = Math.round(sampleRate * 0.2);

  constructor() {
    super();
    this.port.onmessage = (event: MessageEvent<unknown>) => {
      const message = decodePlayerMessage(event.data);
      if (!message) return;
      if (message.type === "reset") {
        this.reset();
        return;
      }
      const firstChannel = message.channels[0];
      if (!firstChannel) return;
      const frames = firstChannel.length;
      if (
        frames === 0 ||
        message.channels.some((channel) => channel.length !== frames)
      )
        return;
      const scheduled =
        message.startFrame !== null && message.captureTimestamp !== null;
      if (this.scheduled !== null && this.scheduled !== scheduled) {
        this.reset();
        this.port.postMessage({ type: "resync", queuedFrames: 0 });
      }
      this.scheduled = scheduled;
      this.queue.push({
        channels: message.channels,
        offset: 0,
        startFrame: scheduled ? message.startFrame : null,
        captureTimestamp: scheduled ? message.captureTimestamp : null,
      });
      this.queuedFrames += frames;
      if (!scheduled && this.queuedFrames > this.maximumFrames) {
        this.dropFrames(this.queuedFrames - this.targetFrames);
        this.playing = this.queuedFrames >= this.targetFrames;
        this.port.postMessage({
          type: "resync",
          queuedFrames: this.queuedFrames,
        });
      }
    };
  }

  private reset(): void {
    this.queue = [];
    this.queuedFrames = 0;
    this.playing = false;
    this.scheduled = null;
    this.lastCaptureTimestamp = null;
    this.lastCaptureFrame = null;
  }

  private dropFrames(count: number): void {
    let remaining = count;
    while (remaining > 0) {
      const chunk = this.queue[0];
      const firstChannel = chunk?.channels[0];
      if (!chunk || !firstChannel) break;
      const available = firstChannel.length - chunk.offset;
      const dropped = Math.min(remaining, available);
      chunk.offset += dropped;
      this.queuedFrames -= dropped;
      remaining -= dropped;
      if (chunk.offset === firstChannel.length) this.queue.shift();
    }
  }

  process(_inputs: Float32Array[][], outputs: Float32Array[][]): boolean {
    const output = outputs[0] ?? [];
    const frameCount = output[0]?.length ?? 0;
    for (const channel of output) channel.fill(0);
    if (this.scheduled) {
      this.processScheduled(output, frameCount);
      this.report();
      return true;
    }
    if (!this.playing) {
      this.playing = this.queuedFrames >= this.targetFrames;
      if (!this.playing) {
        this.report();
        return true;
      }
    }

    let outputOffset = 0;
    while (outputOffset < frameCount) {
      const chunk = this.queue[0];
      const firstChannel = chunk?.channels[0];
      if (!chunk || !firstChannel) break;
      const available = firstChannel.length - chunk.offset;
      const copied = Math.min(frameCount - outputOffset, available);
      for (let index = 0; index < output.length; index += 1) {
        const source =
          chunk.channels[Math.min(index, chunk.channels.length - 1)];
        const destination = output[index];
        if (source && destination)
          destination.set(
            source.subarray(chunk.offset, chunk.offset + copied),
            outputOffset,
          );
      }
      chunk.offset += copied;
      outputOffset += copied;
      this.queuedFrames -= copied;
      if (chunk.offset === firstChannel.length) this.queue.shift();
    }
    if (outputOffset < frameCount) {
      this.playing = false;
      this.underflows += 1;
      this.port.postMessage({ type: "underflow", underflows: this.underflows });
    }
    this.report();
    return true;
  }

  private processScheduled(output: Float32Array[], frameCount: number): void {
    const blockStart = currentFrame;
    let outputOffset = 0;
    let wroteSamples = false;
    while (outputOffset < frameCount) {
      const chunk = this.queue[0];
      const firstChannel = chunk?.channels[0];
      if (
        !chunk ||
        !firstChannel ||
        chunk.startFrame === null ||
        chunk.captureTimestamp === null
      )
        break;
      const available = firstChannel.length - chunk.offset;
      const chunkFrame = chunk.startFrame + chunk.offset;
      if (chunkFrame < blockStart + outputOffset) {
        const dropped = Math.min(
          blockStart + outputOffset - chunkFrame,
          available,
        );
        chunk.offset += dropped;
        this.queuedFrames -= dropped;
        if (chunk.offset === firstChannel.length) this.queue.shift();
        continue;
      }
      const destination = chunkFrame - blockStart;
      if (destination >= frameCount) break;
      const copied = Math.min(frameCount - destination, available);
      for (let index = 0; index < output.length; index += 1) {
        const source =
          chunk.channels[Math.min(index, chunk.channels.length - 1)];
        const channel = output[index];
        if (source && channel)
          channel.set(
            source.subarray(chunk.offset, chunk.offset + copied),
            destination,
          );
      }
      chunk.offset += copied;
      outputOffset = destination + copied;
      this.queuedFrames -= copied;
      this.lastCaptureTimestamp =
        chunk.captureTimestamp + (chunk.offset * 1_000_000) / sampleRate;
      this.lastCaptureFrame = blockStart + outputOffset;
      wroteSamples = true;
      if (chunk.offset === firstChannel.length) this.queue.shift();
    }
    const next = this.queue[0];
    if (wroteSamples) {
      this.playing = true;
    } else if (
      this.playing &&
      (!next ||
        (next.startFrame ?? blockStart + frameCount) + next.offset <
          blockStart + frameCount)
    ) {
      this.playing = false;
      this.underflows += 1;
      this.port.postMessage({ type: "underflow", underflows: this.underflows });
    }
  }

  private report(): void {
    this.reportCountdown -= 1;
    if (this.reportCountdown <= 0) {
      this.reportCountdown = 50;
      this.port.postMessage({
        type: "status",
        queuedFrames: this.queuedFrames,
        underflows: this.underflows,
        scheduled: this.scheduled === true,
        captureTimestamp: this.lastCaptureTimestamp,
        captureFrame: this.lastCaptureFrame,
      });
    }
  }
}

registerProcessor("waymote-audio-player", WaymoteAudioPlayer);
