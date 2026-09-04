import RFB from "@novnc/novnc";

import {
  ConnectionController,
  type ConnectionState,
  type RfbConnection,
} from "./controller";

const labels: Record<ConnectionState, string> = {
  booting: "Starting",
  connecting: "Connecting",
  connected: "Connected",
  "retry-wait": "Waiting to retry",
  offline: "Browser offline",
  "stopped-by-user": "Disconnected",
  failed: "Could not connect",
};

export class DesktopSession {
  state = $state<ConnectionState>("booting");
  error = $state<string | null>(null);

  #controller: ConnectionController | null = null;
  #target: HTMLElement | null = null;
  #frameRequest: number | null = null;
  #startedAt = 0;

  get label(): string {
    return labels[this.state];
  }

  get connected(): boolean {
    return this.state === "connected";
  }

  mount(target: HTMLElement): void {
    this.#target = target;
    this.#controller = new ConnectionController(() => this.#createRfb(), {
      onChange: (state) => this.#handleState(state),
    });
    window.addEventListener("online", this.#handleOnline);
    window.addEventListener("offline", this.#handleOffline);
    this.#controller.mount(navigator.onLine);
  }

  disconnect(): void {
    this.#controller?.disconnect();
  }

  reconnect(): void {
    this.error = null;
    this.#controller?.reconnect();
  }

  retry(): void {
    this.error = null;
    this.#controller?.retry();
  }

  reauthenticate(): void {
    window.location.assign(window.location.origin);
  }

  sendCtrlAltDel(): void {
    this.#controller?.connection?.sendCtrlAltDel();
  }

  async paste(): Promise<void> {
    const connection = this.#controller?.connection;
    if (!connection) return;
    try {
      const text = await navigator.clipboard.readText();
      if (connection !== this.#controller?.connection || !this.connected)
        return;
      connection.clipboardPasteFrom(text);
      this.error = null;
    } catch (error) {
      this.error =
        error instanceof Error
          ? `Clipboard read failed: ${error.message}`
          : "Clipboard read failed";
    }
  }

  destroy(): void {
    window.removeEventListener("online", this.#handleOnline);
    window.removeEventListener("offline", this.#handleOffline);
    this.#stopFrameMeasurement();
    this.#controller?.destroy();
    this.#controller = null;
    this.#target = null;
  }

  #createRfb(): RfbConnection {
    if (!this.#target) throw new Error("Desktop viewport is unavailable");
    this.#target.replaceChildren();
    this.#startedAt = performance.now();
    delete this.#target.dataset.connectedMs;
    delete this.#target.dataset.firstFrameMs;
    performance.mark("sprite-desktop-connect-start");
    const protocol = window.location.protocol === "https:" ? "wss:" : "ws:";
    const rfb = new RFB(
      this.#target,
      `${protocol}//${window.location.host}/vnc`,
      {
        shared: true,
      },
    );
    rfb.scaleViewport = true;
    rfb.clipViewport = false;
    rfb.resizeSession = true;
    return rfb;
  }

  #handleState(state: ConnectionState): void {
    this.state = state;
    if (state === "connected") {
      this.error = null;
      performance.mark("sprite-desktop-connected");
      performance.measure(
        "sprite-desktop-connect",
        "sprite-desktop-connect-start",
        "sprite-desktop-connected",
      );
      if (this.#target) {
        this.#target.dataset.connectedMs = String(
          Math.round(performance.now() - this.#startedAt),
        );
        this.#target.focus();
      }
      this.#measureFirstFrame();
    } else {
      this.#stopFrameMeasurement();
    }
  }

  #measureFirstFrame(): void {
    this.#stopFrameMeasurement();
    const inspect = () => {
      // Timing observation is bounded and never disconnects a dark desktop.
      if (
        this.state !== "connected" ||
        !this.#target ||
        performance.now() - this.#startedAt > 15_000
      ) {
        this.#frameRequest = null;
        return;
      }
      const canvas = this.#target.querySelector("canvas");
      if (canvas && canvas.width > 0 && canvas.height > 0) {
        const context = canvas.getContext("2d", { willReadFrequently: true });
        if (context) {
          const width = Math.min(canvas.width, 32);
          const height = Math.min(canvas.height, 32);
          const pixels = context.getImageData(0, 0, width, height).data;
          let nonBlack = false;
          for (let index = 0; index < pixels.length; index += 4) {
            if (pixels[index] || pixels[index + 1] || pixels[index + 2]) {
              nonBlack = true;
              break;
            }
          }
          if (nonBlack) {
            performance.mark("sprite-desktop-first-frame");
            performance.measure(
              "sprite-desktop-first-frame",
              "sprite-desktop-connect-start",
              "sprite-desktop-first-frame",
            );
            this.#target.dataset.firstFrameMs = String(
              Math.round(performance.now() - this.#startedAt),
            );
            this.#frameRequest = null;
            return;
          }
        }
      }
      this.#frameRequest = requestAnimationFrame(inspect);
    };
    this.#frameRequest = requestAnimationFrame(inspect);
  }

  #stopFrameMeasurement(): void {
    if (this.#frameRequest !== null) cancelAnimationFrame(this.#frameRequest);
    this.#frameRequest = null;
  }

  #handleOnline = () => this.#controller?.setOnline(true);
  #handleOffline = () => this.#controller?.setOnline(false);
}
