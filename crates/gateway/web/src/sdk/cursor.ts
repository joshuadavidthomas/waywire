import type { CursorState } from "./messages.ts";

export class LocalCursor {
  private readonly element: HTMLElement;
  private readonly original: string;
  private readonly priority: string;
  private active = false;
  private disposed = false;
  private state: CursorState | null = null;

  constructor(element: HTMLElement) {
    this.element = element;
    this.original = element.style.getPropertyValue("cursor");
    this.priority = element.style.getPropertyPriority("cursor");
  }

  private restore(): void {
    if (this.original)
      this.element.style.setProperty("cursor", this.original, this.priority);
    else this.element.style.removeProperty("cursor");
  }

  setActive(active: boolean): void {
    if (this.disposed) return;
    this.active = active;
    this.refresh();
  }

  clear(): void {
    if (this.disposed) return;
    this.active = false;
    this.state = null;
    this.restore();
  }

  dispose(): void {
    if (this.disposed) return;
    this.active = false;
    this.state = null;
    this.restore();
    this.disposed = true;
  }

  update(state: CursorState): void {
    if (this.disposed) return;
    this.state = state;
    this.refresh();
  }

  refresh(): void {
    if (this.disposed) return;
    if (!this.active || !this.state) {
      this.restore();
    } else if (!this.state.visible) {
      this.element.style.setProperty("cursor", "none");
    } else {
      this.element.style.setProperty("cursor", this.state.shape);
    }
  }

  position(): CursorState["position"] {
    return this.state?.position ?? null;
  }
}
