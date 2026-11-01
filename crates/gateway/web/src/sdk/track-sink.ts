type TrackGenerator = MediaStreamTrack & {
  readonly writable: WritableStream<VideoFrame>;
};
type TrackGeneratorConstructor = new (init: {
  kind: "video";
}) => TrackGenerator;

/**
 * Shows decoded frames through a `<video>` laid over the canvas, where the
 * browser has `MediaStreamTrackGenerator` (Chromium). The browser composites
 * the track's frames itself, so a frame skips the synchronous canvas
 * `drawImage` on the main thread. Elsewhere the viewer draws into the canvas.
 */
export class TrackSink {
  private constructor(
    private readonly element: HTMLVideoElement,
    private readonly track: TrackGenerator,
    private readonly writer: WritableStreamDefaultWriter<VideoFrame>,
    private readonly observer: ResizeObserver,
  ) {}

  static over(canvas: HTMLCanvasElement): TrackSink | null {
    const Generator = (
      globalThis as { MediaStreamTrackGenerator?: TrackGeneratorConstructor }
    ).MediaStreamTrackGenerator;
    if (typeof Generator !== "function" || !canvas.parentElement) return null;
    const track = new Generator({ kind: "video" });
    const element = document.createElement("video");
    element.muted = true;
    element.autoplay = true;
    element.playsInline = true;
    element.setAttribute("aria-hidden", "true");
    element.srcObject = new MediaStream([track]);
    // Input stays on the canvas; the video only covers its box and letterboxes
    // the picture the same way.
    const { objectFit, objectPosition } = getComputedStyle(canvas);
    Object.assign(element.style, {
      position: "absolute",
      pointerEvents: "none",
      objectFit,
      objectPosition,
    });
    const place = () =>
      Object.assign(element.style, {
        left: `${canvas.offsetLeft}px`,
        top: `${canvas.offsetTop}px`,
        width: `${canvas.offsetWidth}px`,
        height: `${canvas.offsetHeight}px`,
      });
    place();
    const observer = new ResizeObserver(place);
    observer.observe(canvas);
    canvas.after(element);
    element.play().catch(() => undefined);
    return new TrackSink(element, track, track.writable.getWriter(), observer);
  }

  /** Show the frame; the caller keeps its own reference. */
  show(frame: VideoFrame): void {
    this.writer.write(frame.clone()).catch(() => undefined);
  }

  close(): void {
    this.observer.disconnect();
    this.writer.close().catch(() => undefined);
    this.track.stop();
    this.element.remove();
  }
}
