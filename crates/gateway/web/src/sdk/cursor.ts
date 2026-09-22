import type { CursorState } from "./messages.ts";

type VideoDimensions = Readonly<{
  width: number;
  height: number;
}>;

const normalizedPointerExtent = 65_535;

export class LocalCursor {
  private readonly element: HTMLElement;
  private readonly display: HTMLCanvasElement;
  private readonly overlay: HTMLElement;
  private readonly original: string;
  private readonly priority: string;
  private readonly resizeObserver: ResizeObserver | null;
  private controlling = false;
  private pointerLocked = false;
  private disposed = false;
  private state: CursorState | null = null;
  private dimensions: VideoDimensions | null = null;

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
      backgroundImage: `url("data:image/svg+xml,${encodeURIComponent(
        '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 24"><path d="M1 1v20l4-5 4 6 3-2-4-6h7z" fill="white" stroke="black" stroke-linejoin="round"/></svg>',
      )}")`,
      pointerEvents: "none",
      zIndex: "2147483647",
    });
    document.body.append(this.overlay);
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
    this.restore();
    this.hideOverlay();
  }

  dispose(): void {
    if (this.disposed) return;
    this.controlling = false;
    this.state = null;
    this.dimensions = null;
    this.restore();
    this.hideOverlay();
    this.resizeObserver?.disconnect();
    window.removeEventListener("resize", this.refreshLayout);
    window.removeEventListener("scroll", this.refreshLayout, true);
    document.removeEventListener(
      "fullscreenchange",
      this.handleFullscreenChange,
    );
    this.overlay.remove();
    this.disposed = true;
  }

  update(state: CursorState): void {
    if (this.disposed) return;
    this.state = state;
    this.refresh();
  }

  refresh(): void {
    if (this.disposed) return;
    if (!this.controlling || !this.state) {
      this.restore();
    } else if (this.pointerLocked || !this.state.visible) {
      this.element.style.setProperty("cursor", "none");
    } else {
      this.element.style.setProperty("cursor", this.state.shape);
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
    if (
      this.disposed ||
      (this.controlling && !this.pointerLocked) ||
      !state?.visible ||
      !state.position ||
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
    const x = left + (state.position.x / normalizedPointerExtent) * width;
    const y = top + (state.position.y / normalizedPointerExtent) * height;
    this.overlay.style.transform = `translate3d(${x}px, ${y}px, 0)`;
    this.overlay.style.display = "block";
  }

  position(): CursorState["position"] {
    return this.state?.position ?? null;
  }
}
