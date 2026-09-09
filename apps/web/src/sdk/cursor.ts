import type { CursorState } from "./messages.ts";

export type CursorScale = Readonly<{ x: number; y: number }>;

function toError(cause: unknown): Error {
  return cause instanceof Error ? cause : new Error(String(cause));
}

// Cursor position belongs to the browser. Only its image comes from the server.
export class LocalCursor {
  private readonly element: HTMLElement;
  private readonly scale: () => CursorScale;
  private readonly onError: (error: Error) => void;
  private readonly original: string;
  private readonly priority: string;
  private active = false;
  private disposed = false;
  private version = 0;
  private state: CursorState | null = null;
  private bitmap: HTMLImageElement | null = null;
  private imageURL = "";
  private cachedBitmap: HTMLImageElement | null = null;
  private cachedKey = "";
  private cachedCSS = "";

  constructor(
    element: HTMLElement,
    scale: () => CursorScale,
    onError: (error: Error) => void,
  ) {
    this.element = element;
    this.scale = scale;
    this.onError = onError;
    this.original = element.style.getPropertyValue("cursor");
    this.priority = element.style.getPropertyPriority("cursor");
  }

  restore(): void {
    if (this.original)
      this.element.style.setProperty("cursor", this.original, this.priority);
    else this.element.style.removeProperty("cursor");
  }

  setActive(active: boolean): void {
    this.active = active;
    this.refresh();
  }

  clear(): void {
    this.version++;
    this.active = false;
    this.state = null;
    this.bitmap = null;
    this.imageURL = "";
    this.cachedBitmap = null;
    this.restore();
  }

  dispose(): void {
    this.clear();
    this.disposed = true;
  }

  update(state: CursorState): void {
    if (this.disposed) return;
    const integers = [
      state.width,
      state.height,
      state.hotspotX,
      state.hotspotY,
    ];
    if (
      typeof state.visible !== "boolean" ||
      !integers.every(Number.isSafeInteger) ||
      typeof state.image !== "string" ||
      state.image.length > 400000 ||
      (state.image !== "" &&
        (!/^data:image\/png;base64,[A-Za-z0-9+/=]+$/.test(state.image) ||
          state.width < 1 ||
          state.height < 1 ||
          state.width > 256 ||
          state.height > 256))
    ) {
      throw new TypeError("Invalid remote cursor image");
    }
    this.state = state;
    const version = ++this.version;
    this.refresh();
    if (!state.image || (this.bitmap && this.imageURL === state.image)) return;
    const bitmap = new Image();
    bitmap.src = state.image;
    bitmap
      .decode()
      .then(() => {
        if (this.disposed || version !== this.version) return;
        if (
          bitmap.naturalWidth !== state.width ||
          bitmap.naturalHeight !== state.height
        ) {
          throw new Error("Remote cursor dimensions do not match its image");
        }
        this.bitmap = bitmap;
        this.imageURL = state.image;
        this.refresh();
      })
      .catch((error) => {
        if (this.disposed || version !== this.version) return;
        this.state = null;
        this.restore();
        this.onError(toError(error));
      });
  }

  refresh(): void {
    if (this.disposed) return;
    const state = this.state;
    if (!this.active || !state || !state.image) {
      this.restore();
      return;
    }
    if (!state.visible) {
      this.element.style.setProperty("cursor", "none");
      return;
    }
    if (!this.bitmap || this.imageURL !== state.image) {
      this.restore();
      return;
    }
    const scale = this.scale();
    if (!(scale.x > 0 && scale.y > 0)) {
      this.restore();
      return;
    }
    const browserCursorLimit = 128;
    const limit = Math.min(
      1,
      browserCursorLimit / (state.width * scale.x),
      browserCursorLimit / (state.height * scale.y),
    );
    const width = Math.max(1, Math.round(state.width * scale.x * limit));
    const height = Math.max(1, Math.round(state.height * scale.y * limit));
    const x = Math.max(
      0,
      Math.min(width - 1, Math.round((state.hotspotX * width) / state.width)),
    );
    const y = Math.max(
      0,
      Math.min(
        height - 1,
        Math.round((state.hotspotY * height) / state.height),
      ),
    );
    const key = `${width},${height},${x},${y}`;
    if (this.cachedBitmap !== this.bitmap || this.cachedKey !== key) {
      const canvas = document.createElement("canvas");
      canvas.width = width;
      canvas.height = height;
      const context = canvas.getContext("2d");
      if (!context)
        throw new Error("Cursor canvas does not provide a 2D context");
      context.drawImage(this.bitmap, 0, 0, width, height);
      this.cachedCSS = `url("${canvas.toDataURL("image/png")}") ${x} ${y}, default`;
      this.cachedBitmap = this.bitmap;
      this.cachedKey = key;
    }
    this.element.style.setProperty("cursor", this.cachedCSS);
  }
}
