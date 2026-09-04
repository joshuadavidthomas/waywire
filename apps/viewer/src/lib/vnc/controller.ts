export type ConnectionState =
  | "booting"
  | "connecting"
  | "connected"
  | "retry-wait"
  | "offline"
  | "stopped-by-user"
  | "failed";

export interface RfbConnection {
  addEventListener(type: "connect", listener: () => void): void;
  addEventListener(
    type: "disconnect",
    listener: (event: CustomEvent<{ clean: boolean }>) => void,
  ): void;
  disconnect(): void;
  sendCtrlAltDel(): void;
  clipboardPasteFrom(text: string): void;
}

type RfbFactory = () => RfbConnection;
type TimerHandle = number;

export interface Clock {
  now(): number;
  setTimeout(callback: () => void, delay: number): TimerHandle;
  clearTimeout(timer: TimerHandle): void;
}

const browserClock: Clock = {
  now: () => performance.now(),
  setTimeout: (callback, delay) => window.setTimeout(callback, delay),
  clearTimeout: (timer) => window.clearTimeout(timer),
};

interface ControllerOptions {
  clock?: Clock;
  random?: () => number;
  onChange?: (state: ConnectionState) => void;
}

export class ConnectionController {
  state: ConnectionState = "booting";
  #online = true;

  #generation = 0;
  #connection: RfbConnection | null = null;
  #retryTimer: TimerHandle | null = null;
  #budgetTimer: TimerHandle | null = null;
  #negotiationTimer: TimerHandle | null = null;
  #deadline: number | null = null;
  #remaining = 0;
  #retryDelay = 250;
  readonly #factory: RfbFactory;
  readonly #clock: Clock;
  readonly #random: () => number;
  readonly #onChange: (state: ConnectionState) => void;

  constructor(factory: RfbFactory, options: ControllerOptions = {}) {
    this.#factory = factory;
    this.#clock = options.clock ?? browserClock;
    this.#random = options.random ?? Math.random;
    this.#onChange = options.onChange ?? (() => undefined);
  }

  get connection(): RfbConnection | null {
    return this.#connection;
  }

  mount(online = true): void {
    if (this.state !== "booting") return;
    this.#online = online;
    this.#startCycle();
  }

  setOnline(online: boolean): void {
    this.#online = online;
    if (!online) {
      if (
        this.state === "stopped-by-user" ||
        this.state === "failed" ||
        this.state === "offline"
      )
        return;
      if (this.state === "connected") this.#freshBudget();
      this.#pauseOffline();
      return;
    }
    if (this.state !== "offline") return;
    this.#resumeBudget();
    this.#attempt();
  }

  disconnect(): void {
    this.#invalidate();
    this.#deadline = null;
    this.#remaining = 0;
    this.#setState("stopped-by-user");
  }

  reconnect(): void {
    if (this.state !== "stopped-by-user") return;
    this.#startCycle();
  }

  retry(): void {
    if (this.state !== "failed") return;
    this.#startCycle();
  }

  destroy(): void {
    this.disconnect();
  }

  #startCycle(): void {
    this.#freshBudget();
    if (this.#online) this.#attempt();
    else this.#pauseOffline();
  }

  #freshBudget(): void {
    this.#remaining = 60_000;
    this.#retryDelay = 250;
    this.#resumeBudget();
  }

  #resumeBudget(): void {
    this.#deadline = this.#clock.now() + this.#remaining;
    this.#clearTimer("budget");
    const deadline = this.#deadline;
    this.#budgetTimer = this.#clock.setTimeout(() => {
      if (deadline !== this.#deadline) return;
      this.#fail();
    }, this.#remaining);
  }

  #pauseOffline(): void {
    if (this.#deadline !== null) {
      this.#remaining = Math.max(0, this.#deadline - this.#clock.now());
    }
    this.#invalidate();
    this.#deadline = null;
    this.#setState("offline");
  }

  #attempt(): void {
    if (this.#deadline === null || this.#clock.now() >= this.#deadline) {
      this.#fail();
      return;
    }

    this.#invalidateAttempt();
    const generation = this.#generation;
    this.#setState("connecting");
    this.#negotiationTimer = this.#clock.setTimeout(() => {
      if (generation !== this.#generation) return;
      this.#retry();
    }, 10_000);

    let connection: RfbConnection;
    try {
      connection = this.#factory();
    } catch {
      this.#retry();
      return;
    }
    this.#connection = connection;
    connection.addEventListener("connect", () => {
      if (generation !== this.#generation || this.state !== "connecting")
        return;
      if (this.#deadline === null || this.#clock.now() >= this.#deadline) {
        this.#fail();
        return;
      }
      this.#clearTimer("negotiation");
      this.#clearTimer("budget");
      this.#deadline = null;
      this.#remaining = 0;
      this.#setState("connected");
    });
    connection.addEventListener("disconnect", () => {
      if (generation !== this.#generation) return;
      const wasConnected = this.state === "connected";
      this.#connection = null;
      if (wasConnected) this.#freshBudget();
      this.#retry();
    });
  }

  #retry(): void {
    if (this.#deadline === null || this.#clock.now() >= this.#deadline) {
      this.#fail();
      return;
    }
    this.#invalidateAttempt();
    this.#setState("retry-wait");
    const base = this.#retryDelay;
    this.#retryDelay = Math.min(5_000, base * 2);
    const delay = base * (1 + this.#random() * 0.2);
    const generation = this.#generation;
    this.#retryTimer = this.#clock.setTimeout(() => {
      if (generation === this.#generation) this.#attempt();
    }, delay);
  }

  #fail(): void {
    this.#invalidate();
    this.#deadline = null;
    this.#remaining = 0;
    this.#setState("failed");
  }

  #invalidateAttempt(): void {
    this.#generation += 1;
    this.#clearTimer("retry");
    this.#clearTimer("negotiation");
    const connection = this.#connection;
    this.#connection = null;
    // Invalidate before closing: noVNC may emit disconnect during cleanup.
    connection?.disconnect();
  }

  #invalidate(): void {
    this.#invalidateAttempt();
    this.#clearTimer("budget");
  }

  #clearTimer(kind: "retry" | "budget" | "negotiation"): void {
    if (kind === "retry" && this.#retryTimer !== null) {
      this.#clock.clearTimeout(this.#retryTimer);
      this.#retryTimer = null;
    } else if (kind === "budget" && this.#budgetTimer !== null) {
      this.#clock.clearTimeout(this.#budgetTimer);
      this.#budgetTimer = null;
    } else if (kind === "negotiation" && this.#negotiationTimer !== null) {
      this.#clock.clearTimeout(this.#negotiationTimer);
      this.#negotiationTimer = null;
    }
  }

  #setState(state: ConnectionState): void {
    this.state = state;
    this.#onChange(state);
  }
}
