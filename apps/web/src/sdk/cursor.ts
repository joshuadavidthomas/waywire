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
  private decoded: HTMLImageElement | null = null;
  private imageURL = "";
  private cachedDecoded: HTMLImageElement | null = null;
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
    this.decoded = null;
    this.imageURL = "";
    this.cachedDecoded = null;
    this.restore();
  }

  dispose(): void {
    this.clear();
    this.disposed = true;
  }

  update(state: CursorState): void {
    if (this.disposed) return;
    if (typeof state.visible !== "boolean") {
      throw new TypeError("Invalid remote cursor state");
    }
    const bitmap = state.bitmap;
    if (
      bitmap &&
      (![bitmap.width, bitmap.height, bitmap.hotspot.x, bitmap.hotspot.y].every(
        Number.isSafeInteger,
      ) ||
        typeof bitmap.image !== "string" ||
        bitmap.image.length > 400000 ||
        !/^data:image\/png;base64,[A-Za-z0-9+/=]+$/.test(bitmap.image) ||
        bitmap.width < 1 ||
        bitmap.height < 1 ||
        bitmap.width > 256 ||
        bitmap.height > 256)
    ) {
      throw new TypeError("Invalid remote cursor image");
    }
    this.state = state;
    const version = ++this.version;
    this.refresh();
    if (!bitmap || (this.decoded && this.imageURL === bitmap.image)) return;
    const decoded = new Image();
    decoded.src = bitmap.image;
    decoded
      .decode()
      .then(() => {
        if (this.disposed || version !== this.version) return;
        if (
          decoded.naturalWidth !== bitmap.width ||
          decoded.naturalHeight !== bitmap.height
        ) {
          throw new Error("Remote cursor dimensions do not match its image");
        }
        this.decoded = decoded;
        this.imageURL = bitmap.image;
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
    const bitmap = state?.bitmap;
    if (!this.active || !state || !bitmap) {
      this.restore();
      return;
    }
    if (!state.visible) {
      this.element.style.setProperty("cursor", "none");
      return;
    }
    if (!this.decoded || this.imageURL !== bitmap.image) {
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
      browserCursorLimit / (bitmap.width * scale.x),
      browserCursorLimit / (bitmap.height * scale.y),
    );
    const width = Math.max(1, Math.round(bitmap.width * scale.x * limit));
    const height = Math.max(1, Math.round(bitmap.height * scale.y * limit));
    const x = Math.max(
      0,
      Math.min(
        width - 1,
        Math.round((bitmap.hotspot.x * width) / bitmap.width),
      ),
    );
    const y = Math.max(
      0,
      Math.min(
        height - 1,
        Math.round((bitmap.hotspot.y * height) / bitmap.height),
      ),
    );
    const key = `${width},${height},${x},${y}`;
    if (this.cachedDecoded !== this.decoded || this.cachedKey !== key) {
      const canvas = document.createElement("canvas");
      canvas.width = width;
      canvas.height = height;
      const context = canvas.getContext("2d");
      if (!context)
        throw new Error("Cursor canvas does not provide a 2D context");
      context.drawImage(this.decoded, 0, 0, width, height);
      this.cachedCSS = `url("${canvas.toDataURL("image/png")}") ${x} ${y}, default`;
      this.cachedDecoded = this.decoded;
      this.cachedKey = key;
    }
    this.element.style.setProperty("cursor", this.cachedCSS);
  }
}
