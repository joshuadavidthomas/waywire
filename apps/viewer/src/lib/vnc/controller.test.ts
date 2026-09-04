import { describe, expect, it } from "vitest";

import {
  ConnectionController,
  type Clock,
  type RfbConnection,
} from "./controller";

class FakeClock implements Clock {
  time = 0;
  #next = 1;
  #timers = new Map<number, { at: number; callback: () => void }>();

  now(): number {
    return this.time;
  }

  setTimeout(callback: () => void, delay: number): number {
    const id = this.#next++;
    this.#timers.set(id, { at: this.time + delay, callback });
    return id;
  }

  clearTimeout(timer: number): void {
    this.#timers.delete(timer);
  }

  get pending(): number {
    return this.#timers.size;
  }

  advance(milliseconds: number): void {
    const end = this.time + milliseconds;
    while (true) {
      const due = [...this.#timers.entries()]
        .filter(([, timer]) => timer.at <= end)
        .sort((left, right) => left[1].at - right[1].at)[0];
      if (!due) break;
      this.time = due[1].at;
      this.#timers.delete(due[0]);
      due[1].callback();
    }
    this.time = end;
  }
}

class FakeRfb implements RfbConnection {
  disconnected = 0;
  #connect: (() => void)[] = [];
  #disconnect: ((event: CustomEvent<{ clean: boolean }>) => void)[] = [];

  addEventListener(
    type: "connect" | "disconnect",
    listener: (() => void) | ((event: CustomEvent<{ clean: boolean }>) => void),
  ): void {
    if (type === "connect") this.#connect.push(listener as () => void);
    else
      this.#disconnect.push(
        listener as (event: CustomEvent<{ clean: boolean }>) => void,
      );
  }

  emitConnect(): void {
    for (const listener of this.#connect) listener();
  }

  emitDisconnect(): void {
    for (const listener of this.#disconnect)
      listener({ detail: { clean: false } } as CustomEvent<{ clean: boolean }>);
  }

  disconnect(): void {
    this.disconnected += 1;
  }
  sendCtrlAltDel(): void {}
  clipboardPasteFrom(): void {}
}

const flush = async () => await Promise.resolve();

function setup() {
  const clock = new FakeClock();
  const connections: FakeRfb[] = [];
  const controller = new ConnectionController(
    () => {
      const connection = new FakeRfb();
      connections.push(connection);
      return connection;
    },
    { clock, random: () => 0 },
  );
  return { clock, connections, controller };
}

describe("ConnectionController", () => {
  it("runs genuine negotiation, backoff, and deadline transitions", async () => {
    const { clock, connections, controller } = setup();
    controller.mount();
    await flush();
    expect(controller.state).toBe("connecting");
    clock.advance(10_000);
    expect(controller.state).toBe("retry-wait");
    clock.advance(250);
    await flush();
    expect(connections).toHaveLength(2);
    clock.advance(49_750);
    expect(controller.state).toBe("failed");
    expect(connections[1].disconnected).toBe(1);
  });

  it("does not spend retry budget during a long offline interval", async () => {
    const { clock, connections, controller } = setup();
    controller.mount();
    await flush();
    clock.advance(5_000);
    controller.setOnline(false);
    expect(controller.state).toBe("offline");
    clock.advance(100_000);
    expect(controller.state).toBe("offline");
    controller.setOnline(true);
    await flush();
    expect(controller.state).toBe("connecting");
    expect(connections).toHaveLength(2);
    clock.advance(55_000);
    expect(controller.state).toBe("failed");
  });

  it("ignores stale noVNC events and cancels every timer on Disconnect", () => {
    const { clock, connections, controller } = setup();
    controller.mount();
    controller.disconnect();
    connections[0].emitConnect();
    connections[0].emitDisconnect();
    expect(clock.pending).toBe(0);
    clock.advance(120_000);
    expect(connections).toHaveLength(1);
    expect(controller.state).toBe("stopped-by-user");
    controller.reconnect();
    connections[0].emitConnect();
    expect(controller.state).toBe("connecting");
    connections[1].emitConnect();
    expect(controller.state).toBe("connected");
  });

  it("keeps manual Reconnect paused if the browser is already offline", () => {
    const { clock, connections, controller } = setup();
    controller.mount();
    controller.disconnect();
    controller.setOnline(false);
    controller.reconnect();
    expect(controller.state).toBe("offline");
    expect(clock.pending).toBe(0);
    clock.advance(120_000);
    expect(connections).toHaveLength(1);
    controller.setOnline(true);
    expect(connections).toHaveLength(2);
    clock.advance(60_000);
    expect(controller.state).toBe("failed");
  });

  it("rejects a late connect event after hidden-tab timer throttling", () => {
    const { clock, connections, controller } = setup();
    controller.mount();
    clock.time = 60_001; // Time passed without timers being delivered.
    connections[0].emitConnect();
    expect(controller.state).toBe("failed");
    expect(clock.pending).toBe(0);
  });

  it("survives repeated manual disconnect and reconnect", async () => {
    const { connections, controller } = setup();
    controller.mount();
    await flush();
    for (let index = 0; index < 3; index += 1) {
      controller.disconnect();
      expect(controller.state).toBe("stopped-by-user");
      controller.reconnect();
      await flush();
    }
    expect(connections).toHaveLength(4);
    expect(
      connections.slice(0, 3).every((item) => item.disconnected === 1),
    ).toBe(true);
  });

  it("gives an unexpected disconnect after success a fresh budget", async () => {
    const { clock, connections, controller } = setup();
    controller.mount();
    await flush();
    clock.advance(5_000);
    connections[0].emitConnect();
    expect(controller.state).toBe("connected");
    clock.advance(100_000);
    connections[0].emitDisconnect();
    expect(controller.state).toBe("retry-wait");
    clock.advance(250);
    await flush();
    clock.advance(59_750);
    expect(controller.state).toBe("failed");
  });
});
