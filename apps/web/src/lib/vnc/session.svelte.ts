import type RFB from "@novnc/novnc";
import { DesktopTicket } from "@sprite-desktop/shared/ticket";

export type ConnectionPhase =
  | "idle"
  | "fetching-ticket"
  | "opening-socket"
  | "sending-init"
  | "attaching-rfb"
  | "connected"
  | "disconnected";

const labels: Record<ConnectionPhase, string> = {
  idle: "Ready to connect",
  "fetching-ticket": "Fetching ticket",
  "opening-socket": "Opening socket",
  "sending-init": "Sending proxy target",
  "attaching-rfb": "Starting RFB",
  connected: "Desktop connected",
  disconnected: "Desktop disconnected",
};

function waitForOpen(socket: WebSocket): Promise<void> {
  return new Promise((resolve, reject) => {
    const timeout = window.setTimeout(
      () => finish(new Error("WebSocket open timed out")),
      10_000,
    );

    const finish = (error?: Error) => {
      window.clearTimeout(timeout);
      socket.removeEventListener("open", handleOpen);
      socket.removeEventListener("error", handleError);
      socket.removeEventListener("close", handleClose);
      if (error) reject(error);
      else resolve();
    };
    const handleOpen = () => finish();
    const handleError = () => finish(new Error("WebSocket open failed"));
    const handleClose = () =>
      finish(new Error("WebSocket closed before opening"));

    socket.addEventListener("open", handleOpen, { once: true });
    socket.addEventListener("error", handleError, { once: true });
    socket.addEventListener("close", handleClose, { once: true });
  });
}

function waitForProxyAcknowledgement(socket: WebSocket): Promise<void> {
  return new Promise((resolve, reject) => {
    const rfbMessageHandler = socket.onmessage;
    if (!rfbMessageHandler) {
      reject(new Error("noVNC did not attach its WebSocket handler"));
      return;
    }

    const timeout = window.setTimeout(
      () => finish(new Error("Sprites proxy acknowledgement timed out")),
      10_000,
    );

    const finish = (error?: Error) => {
      window.clearTimeout(timeout);
      socket.removeEventListener("error", handleError);
      socket.removeEventListener("close", handleClose);
      if (socket.onmessage === handleMessage)
        socket.onmessage = rfbMessageHandler;
      if (error) reject(error);
      else resolve();
    };
    const handleMessage = (event: MessageEvent<unknown>) => {
      if (typeof event.data !== "string") {
        finish(
          new Error("Sprites proxy sent RFB data before its acknowledgement"),
        );
        return;
      }

      try {
        const acknowledgement: unknown = JSON.parse(event.data);
        if (
          typeof acknowledgement !== "object" ||
          acknowledgement === null ||
          !("status" in acknowledgement) ||
          acknowledgement.status !== "connected"
        ) {
          finish(new Error("Sprites proxy rejected the TCP target"));
          return;
        }
        finish();
      } catch {
        finish(new Error("Sprites proxy sent an invalid acknowledgement"));
      }
    };
    const handleError = () =>
      finish(new Error("Sprites proxy connection failed"));
    const handleClose = () =>
      finish(new Error("Sprites proxy closed before connecting"));

    socket.onmessage = handleMessage;
    socket.addEventListener("error", handleError, { once: true });
    socket.addEventListener("close", handleClose, { once: true });
  });
}

export class DesktopSession {
  phase = $state<ConnectionPhase>("idle");
  error = $state<string | null>(null);
  reason = $state<string | null>(null);

  #rfb: RFB | null = null;
  #socket: WebSocket | null = null;
  #generation = 0;

  get label(): string {
    return labels[this.phase];
  }

  get connected(): boolean {
    return this.phase === "connected";
  }

  get busy(): boolean {
    return !["idle", "connected", "disconnected"].includes(this.phase);
  }

  get canDisconnect(): boolean {
    return this.busy || this.connected;
  }

  async connect(target: HTMLElement): Promise<void> {
    if (this.busy || this.connected) return;

    const generation = ++this.#generation;
    this.error = null;
    this.reason = null;
    target.replaceChildren();

    try {
      this.phase = "fetching-ticket";
      const response = await fetch("/api/desktop/ticket", {
        headers: { Accept: "application/json" },
      });
      if (!response.ok)
        throw new Error(`Ticket request failed (${response.status})`);

      const ticket = DesktopTicket.parse(await response.json());
      const { default: RFBClient } = await import("@novnc/novnc");
      if (generation !== this.#generation) return;

      this.phase = "opening-socket";
      const socket = new WebSocket(ticket.wsUrl);
      socket.binaryType = "arraybuffer";
      this.#socket = socket;
      await waitForOpen(socket);
      if (generation !== this.#generation) {
        socket.close(1000, "Connection cancelled");
        return;
      }

      this.phase = "sending-init";
      // Attach first, then replace noVNC's message handler long enough to consume
      // the proxy's text acknowledgement. This keeps the following RFB banner
      // from arriving during an unattached await gap.
      const rfb = new RFBClient(target, socket, { shared: true });
      rfb.scaleViewport = true;
      rfb.clipViewport = false;
      rfb.resizeSession = true;
      this.#rfb = rfb;

      rfb.addEventListener("connect", () => {
        if (generation !== this.#generation) return;
        this.phase = "connected";
        target.focus();
      });
      rfb.addEventListener("disconnect", (event) => {
        if (generation !== this.#generation) return;
        this.#rfb = null;
        this.#socket = null;
        this.phase = "disconnected";
        this.reason = event.detail.clean
          ? "The VNC session closed cleanly."
          : "The VNC session ended unexpectedly.";
      });

      const proxyReady = waitForProxyAcknowledgement(socket);
      socket.send(JSON.stringify({ host: "localhost", port: 5900 }));
      await proxyReady;
      if (generation !== this.#generation) {
        rfb.disconnect();
        return;
      }
      this.phase = "attaching-rfb";
    } catch (error) {
      if (generation !== this.#generation) return;
      this.#rfb = null;
      this.#socket?.close();
      this.#socket = null;
      this.phase = "disconnected";
      this.error = error instanceof Error ? error.message : "Connection failed";
      this.reason = "The desktop could not be reached.";
    }
  }

  disconnect(): void {
    if (!this.canDisconnect) return;
    this.#generation += 1;
    const rfb = this.#rfb;
    const socket = this.#socket;
    this.#rfb = null;
    this.#socket = null;

    if (rfb) rfb.disconnect();
    else if (socket && socket.readyState < WebSocket.CLOSING)
      socket.close(1000, "Disconnected by user");

    this.phase = "disconnected";
    this.error = null;
    this.reason = "Disconnected by user.";
  }

  sendCtrlAltDel(): void {
    this.#rfb?.sendCtrlAltDel();
  }

  async paste(): Promise<void> {
    if (!this.#rfb) return;
    try {
      const text = await navigator.clipboard.readText();
      this.#rfb.clipboardPasteFrom(text);
      this.error = null;
    } catch (error) {
      this.error =
        error instanceof Error
          ? `Clipboard read failed: ${error.message}`
          : "Clipboard read failed";
    }
  }

  destroy(): void {
    if (this.canDisconnect) this.disconnect();
  }
}
