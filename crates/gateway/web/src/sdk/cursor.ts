import type { CursorState } from "./messages.ts";

type VideoDimensions = Readonly<{
  width: number;
  height: number;
}>;

const normalizedPointerExtent = 65_535;
const glideDurationMs = 120;
type Point = Readonly<{ x: number; y: number }>;
type Glide = { from: Point; started: number; deadline: number };
const defaultCursorImage = `url("data:image/svg+xml,${encodeURIComponent(
  '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 24"><path d="M1 1v20l4-5 4 6 3-2-4-6h7z" fill="white" stroke="black" stroke-linejoin="round"/></svg>',
)}")`;

export class LocalCursor {
  private readonly element: HTMLElement;
  private readonly display: HTMLCanvasElement;
  private readonly overlay: HTMLElement;
  private readonly pulse: HTMLElement;
  private readonly reducedMotion: MediaQueryList | null;
  private readonly original: string;
  private readonly priority: string;
  private readonly resizeObserver: ResizeObserver | null;
  private controlling = false;
  private pointerLocked = false;
  private disposed = false;
  private state: CursorState | null = null;
  private dimensions: VideoDimensions | null = null;
  private displayedPosition: Point | null = null;
  private glide: Glide | null = null;
  private animationFrame: number | null = null;
  private pulseAnimation: Animation | null = null;

  constructor(element: HTMLElement, display: HTMLCanvasElement) {
    this.element = element;
    this.display = display;
    this.original = element.style.getPropertyValue("cursor");
    this.priority = element.style.getPropertyPriority("cursor");
    this.overlay = document.createElement("div");
    this.overlay.setAttribute("aria-hidden", "true");
    Object.assign(this.overlay.style, {
      position: "fixed",
      display: "none",
      left: "0",
      top: "0",
      width: "16px",
      height: "24px",
      backgroundImage: defaultCursorImage,
      pointerEvents: "none",
      zIndex: "2147483647",
    });
    this.pulse = document.createElement("div");
    Object.assign(this.pulse.style, {
      position: "absolute",
      left: "-14px",
      top: "-14px",
      width: "28px",
      height: "28px",
      border: "2px solid #00e5ff",
      borderRadius: "50%",
      opacity: "0",
      pointerEvents: "none",
    });
    this.overlay.append(this.pulse);
    document.body.append(this.overlay);
    this.reducedMotion =
      typeof window.matchMedia === "function"
        ? window.matchMedia("(prefers-reduced-motion: reduce)")
        : null;
    this.reducedMotion?.addEventListener(
      "change",
      this.motionPreferenceChanged,
    );
    window.addEventListener("resize", this.refreshLayout);
    window.addEventListener("scroll", this.refreshLayout, true);
    document.addEventListener("fullscreenchange", this.handleFullscreenChange);
    this.resizeObserver =
      typeof ResizeObserver === "undefined"
        ? null
        : new ResizeObserver(this.refreshLayout);
    this.resizeObserver?.observe(display);
  }

  private restore(): void {
    if (this.original)
      this.element.style.setProperty("cursor", this.original, this.priority);
    else this.element.style.removeProperty("cursor");
  }

  setActive(active: boolean): void {
    if (this.disposed) return;
    this.controlling = active;
    this.refresh();
  }

  setPointerLocked(locked: boolean): void {
    if (this.disposed) return;
    this.pointerLocked = locked;
    this.refresh();
  }

  setVideoDimensions(width: number, height: number): void {
    if (this.disposed) return;
    if (this.dimensions?.width === width && this.dimensions.height === height)
      return;
    this.dimensions = { width, height };
    this.refreshOverlay();
  }

  clear(): void {
    if (this.disposed) return;
    this.controlling = false;
    this.state = null;
    this.resetAnimation();
    this.restore();
    this.hideOverlay();
  }

  dispose(): void {
    if (this.disposed) return;
    this.controlling = false;
    this.state = null;
    this.dimensions = null;
    this.resetAnimation();
    this.restore();
    this.hideOverlay();
    this.resizeObserver?.disconnect();
    window.removeEventListener("resize", this.refreshLayout);
    window.removeEventListener("scroll", this.refreshLayout, true);
    document.removeEventListener(
      "fullscreenchange",
      this.handleFullscreenChange,
    );
    this.reducedMotion?.removeEventListener(
      "change",
      this.motionPreferenceChanged,
    );
    this.overlay.remove();
    this.disposed = true;
  }

  update(state: CursorState): void {
    if (this.disposed) return;
    const previous = this.state?.position;
    const target = state.position;
    const now = performance.now();
    if (this.glide && previous) this.advanceGlide(now, previous);
    const moved =
      target &&
      (!previous || target.x !== previous.x || target.y !== previous.y);
    if (
      !target ||
      target.source === "browser" ||
      this.reducedMotion?.matches ||
      !this.displayedPosition
    ) {
      this.stopGlide();
      this.displayedPosition = target;
    } else if (moved) {
      const deadline =
        this.glide && this.glide.deadline > now
          ? this.glide.deadline
          : now + glideDurationMs;
      this.glide = { from: this.displayedPosition, started: now, deadline };
      if (this.animationFrame === null)
        this.animationFrame = requestAnimationFrame(this.animateGlide);
    }
    this.state = state;
    if (state.click && target?.source === "agent") this.showPulse();
    this.refresh();
  }

  private advanceGlide(now: number, target: Point): void {
    const glide = this.glide;
    if (!glide) return;
    const fraction = Math.min(
      1,
      Math.max(0, (now - glide.started) / (glide.deadline - glide.started)),
    );
    const eased = 1 - (1 - fraction) ** 3;
    this.displayedPosition = {
      x: glide.from.x + (target.x - glide.from.x) * eased,
      y: glide.from.y + (target.y - glide.from.y) * eased,
    };
    if (fraction === 1) this.glide = null;
  }

  private animateGlide = (now: number): void => {
    this.animationFrame = null;
    if (!this.glide || !this.state?.position) return;
    this.advanceGlide(now, this.state.position);
    this.refreshOverlay();
    if (this.glide)
      this.animationFrame = requestAnimationFrame(this.animateGlide);
  };

  private stopGlide(): void {
    if (this.animationFrame !== null) cancelAnimationFrame(this.animationFrame);
    this.animationFrame = null;
    this.glide = null;
  }

  private resetAnimation(): void {
    this.stopGlide();
    this.displayedPosition = null;
    this.pulseAnimation?.cancel();
    this.pulseAnimation = null;
  }

  private motionPreferenceChanged = (): void => {
    if (this.reducedMotion?.matches) {
      this.stopGlide();
      this.displayedPosition = this.state?.position ?? null;
      this.refreshOverlay();
    }
  };

  private showPulse(): void {
    this.pulseAnimation?.cancel();
    this.pulseAnimation = this.pulse.animate(
      this.reducedMotion?.matches
        ? [{ opacity: 0.9 }, { opacity: 0 }]
        : [
            { opacity: 0.9, transform: "scale(0.5)" },
            { opacity: 0, transform: "scale(1.5)" },
          ],
      { duration: 300, easing: "ease-out" },
    );
  }

  refresh(): void {
    if (this.disposed) return;
    if (!this.controlling || !this.state) {
      this.restore();
    } else if (this.pointerLocked || !this.state.visible) {
      this.element.style.setProperty("cursor", "none");
    } else {
      const image = this.state.image;
      this.element.style.setProperty(
        "cursor",
        image
          ? `url("${image.url}") ${image.hotspotX} ${image.hotspotY}, default`
          : this.state.shape,
      );
    }
    this.refreshOverlay();
  }

  refreshLayout = (): void => this.refreshOverlay();

  private handleFullscreenChange = (): void => {
    const fullscreen = document.fullscreenElement;
    // SDK callers should fullscreen a renderable surface container. A canvas
    // can contain only fallback DOM, so its cursor overlay is not reliably
    // rendered when the canvas itself is the fullscreen target.
    const host =
      fullscreen?.contains(this.display) === true ? fullscreen : document.body;
    if (this.overlay.parentElement !== host) host.append(this.overlay);
    this.refreshOverlay();
  };

  private hideOverlay(): void {
    this.overlay.style.display = "none";
  }

  private refreshOverlay(): void {
    const state = this.state;
    const dimensions = this.dimensions;
    const position = this.displayedPosition;
    if (
      this.disposed ||
      (this.controlling && !this.pointerLocked) ||
      !state?.visible ||
      !state.position ||
      !position ||
      !dimensions ||
      dimensions.width <= 0 ||
      dimensions.height <= 0
    ) {
      this.hideOverlay();
      return;
    }
    const bounds = this.display.getBoundingClientRect();
    if (bounds.width <= 0 || bounds.height <= 0) {
      this.hideOverlay();
      return;
    }
    const aspect = dimensions.width / dimensions.height;
    let width = bounds.width;
    let height = bounds.height;
    let left = bounds.left;
    let top = bounds.top;
    if (width / height > aspect) {
      width = height * aspect;
      left += (bounds.width - width) / 2;
    } else {
      height = width / aspect;
      top += (bounds.height - height) / 2;
    }
    const scale = state.image ? width / state.image.outputWidth : 1;
    this.overlay.style.width = `${state.image ? state.image.width * scale : 16}px`;
    this.overlay.style.height = `${state.image ? state.image.height * scale : 24}px`;
    this.overlay.style.backgroundSize = "100% 100%";
    this.overlay.style.backgroundImage = state.image
      ? `url("${state.image.url}")`
      : defaultCursorImage;
    this.overlay.style.filter =
      state.position.source === "agent"
        ? "drop-shadow(0 0 3px #00e5ff) drop-shadow(0 0 2px #00e5ff)"
        : "none";
    const x =
      left +
      (position.x / normalizedPointerExtent) * width -
      (state.image?.hotspotX ?? 0) * scale;
    const y =
      top +
      (position.y / normalizedPointerExtent) * height -
      (state.image?.hotspotY ?? 0) * scale;
    this.overlay.style.transform = `translate3d(${x}px, ${y}px, 0)`;
    this.overlay.style.display = "block";
  }

  position(): CursorState["position"] {
    return this.state?.position ?? null;
  }
}
