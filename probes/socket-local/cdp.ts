import WebSocket from "ws";

export class LocalCdp {
  readonly socket: WebSocket;
  private sequence = 0;
  private pending = new Map<
    number,
    {
      resolve(value: any): void;
      reject(error: Error): void;
      timer: NodeJS.Timeout;
    }
  >();
  onEvent?: (method: string, params: any, sessionId?: string) => void;

  private constructor(socket: WebSocket) {
    this.socket = socket;
    socket.addEventListener("message", (event) => {
      if (
        typeof event.data !== "string" ||
        event.data.length > 16 * 1024 * 1024
      )
        return this.close();
      let message: any;
      try {
        message = JSON.parse(event.data);
      } catch {
        return this.close();
      }
      const pending = this.pending.get(message.id);
      if (!pending) {
        if (message.method)
          this.onEvent?.(message.method, message.params, message.sessionId);
        return;
      }
      clearTimeout(pending.timer);
      this.pending.delete(message.id);
      message.error
        ? pending.reject(new Error(`CDP ${message.error.message}`))
        : pending.resolve(message.result);
    });
    socket.addEventListener("close", () => this.rejectAll());
    socket.addEventListener("error", () => this.rejectAll());
  }

  static async connect(url: string): Promise<LocalCdp> {
    const parsed = new URL(url);
    if (
      parsed.protocol !== "ws:" ||
      parsed.hostname !== "127.0.0.1" ||
      parsed.username ||
      parsed.password
    )
      throw new Error("refusing non-loopback CDP endpoint");
    const socket = new WebSocket(url);
    const cdp = new LocalCdp(socket);
    await new Promise<void>((resolve, reject) => {
      const timer = setTimeout(
        () => reject(new Error("CDP connection timeout")),
        5000,
      );
      socket.addEventListener(
        "open",
        () => {
          clearTimeout(timer);
          resolve();
        },
        { once: true },
      );
      socket.addEventListener(
        "error",
        () => {
          clearTimeout(timer);
          reject(new Error("CDP connection failed"));
        },
        { once: true },
      );
    });
    return cdp;
  }

  call(method: string, params: object = {}, sessionId?: string): Promise<any> {
    if (this.pending.size >= 16 || this.socket.readyState !== WebSocket.OPEN)
      return Promise.reject(new Error("CDP unavailable"));
    const id = ++this.sequence;
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this.pending.delete(id);
        reject(new Error(`CDP timeout: ${method}`));
      }, 30000);
      this.pending.set(id, { resolve, reject, timer });
      this.socket.send(
        JSON.stringify({
          id,
          method,
          params,
          ...(sessionId ? { sessionId } : {}),
        }),
      );
    });
  }

  private rejectAll(): void {
    for (const pending of this.pending.values()) {
      clearTimeout(pending.timer);
      pending.reject(new Error("CDP closed"));
    }
    this.pending.clear();
  }

  close(): void {
    this.rejectAll();
    this.socket.close();
  }
}
