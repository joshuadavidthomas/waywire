import assert from "node:assert/strict";
import test from "node:test";

import { PROTOCOL_VERSION } from "./messages.ts";
import { WaywireSession, type ResizeEvent } from "./session.ts";
import {
  FakeTarget,
  FakeWebSocket,
  flush,
  installBrowser,
  installGlobal,
  installQueueVideoDecoder,
  installResizeObserver,
  socket,
  surfaceOptions,
  videoPacket,
} from "./test-support.ts";

function resizeRecords(socket: FakeWebSocket): ArrayBuffer[] {
  return socket.sent.filter(
    (value): value is ArrayBuffer =>
      value instanceof ArrayBuffer && new DataView(value).getUint8(1) === 6,
  );
}

function resizeSize(record: ArrayBuffer): readonly [number, number] {
  const view = new DataView(record);
  return [view.getUint32(8, true), view.getUint32(12, true)];
}

function resizeRequestID(record: ArrayBuffer): number {
  return new DataView(record).getUint16(18, true);
}

function configureVideo(socket: FakeWebSocket): void {
  socket.dispatch("message", {
    data: JSON.stringify({
      type: "video-config",
      version: PROTOCOL_VERSION,
      codec: "avc1.42E01E",
    }),
  });
}

test("double-click stays unlocked until pointer lock is requested", async () => {
  installBrowser();
  let lockRequests = 0;
  const canvas = new FakeTarget();
  canvas.requestPointerLock = async () => {
    lockRequests += 1;
  };
  const session = new WaywireSession();
  const surface = session.attachSurface(surfaceOptions(canvas));
  try {
    canvas.dispatch("dblclick", {});
    assert.equal(lockRequests, 0);
    await surface.requestPointerLock();
    assert.equal(lockRequests, 1);
  } finally {
    await session.dispose();
  }
});

test("dispose cancels an asynchronous clipboard paste", async () => {
  installBrowser();
  let resolveClipboard: ((text: string) => void) | undefined;
  const clipboardText = new Promise<string>((resolve) => {
    resolveClipboard = resolve;
  });
  installGlobal("navigator", {
    clipboard: { readText: () => clipboardText },
  });
  const canvas = new FakeTarget();
  const session = new WaywireSession();
  session.attachSurface(surfaceOptions(canvas));
  const originalWarn = console.warn;
  const warnings: unknown[][] = [];
  console.warn = (...values: unknown[]) => warnings.push(values);
  try {
    canvas.dispatch("keydown", {
      code: "KeyV",
      ctrlKey: true,
      metaKey: false,
      shiftKey: false,
      repeat: false,
      isComposing: false,
      keyCode: 86,
      preventDefault() {},
      stopPropagation() {},
    });
    const disposal = session.dispose();
    resolveClipboard?.("text after disposal");
    await disposal;
    await clipboardText;
    await Promise.resolve();
    assert.deepEqual(warnings, []);
  } finally {
    console.warn = originalWarn;
  }
});

test("default manual policy never observes or resizes on connect and acquire", async () => {
  const { window } = installBrowser();
  const observers = installResizeObserver();
  const sockets = new Map<string, FakeWebSocket>();
  const session = new WaywireSession({
    endpoint: "https://desktop.example.com",
    createWebSocket(path) {
      const created = new FakeWebSocket();
      sockets.set(path, created);
      return socket(created);
    },
  });
  session.attachSurface(surfaceOptions());
  try {
    assert.deepEqual(session.remoteDisplay.policy, { mode: "manual" });
    assert.equal(observers.length, 0);
    session.input.acquire();
    session.connect();
    await flush();
    const control = sockets.get("/control");
    if (!control) throw new Error("control socket was not created");
    control.readyState = FakeWebSocket.OPEN;
    control.dispatch("open", {});
    control.dispatch("message", {
      data: JSON.stringify({ type: "control-state", state: "active" }),
    });
    window.dispatch("resize", {});
    await new Promise<void>((resolve) => setTimeout(resolve, 110));
    assert.equal(observers.length, 0);
    assert.deepEqual(resizeRecords(control), []);
  } finally {
    await session.dispose();
  }
});

test("an explicit Fit action observes later window resizes", async () => {
  const { window } = installBrowser();
  installGlobal("Element", FakeTarget);
  const observers = installResizeObserver();
  const control = new FakeWebSocket();
  const canvas = new FakeTarget();
  const session = new WaywireSession({
    endpoint: "https://desktop.example.com",
    createWebSocket: (path) =>
      socket(path === "/control" ? control : new FakeWebSocket()),
  });
  session.attachSurface(surfaceOptions(canvas));
  try {
    session.connect();
    await flush();
    control.readyState = FakeWebSocket.OPEN;
    control.dispatch("open", {});

    session.remoteDisplay.fixed({ width: 1600, height: 900, scale: 1 });
    assert.equal(resizeRecords(control).length, 1);
    assert.equal(observers.length, 0);

    session.remoteDisplay.observe({
      element: canvas as unknown as Element,
      devicePixelRatio: 1,
      debounceMs: 0,
    });
    await flush();
    assert.equal(observers.length, 1);
    assert.deepEqual(resizeRecords(control).map(resizeSize), [
      [1600, 900],
      [1280, 720],
    ]);

    canvas.rect.width = 1440;
    canvas.rect.height = 810;
    window.dispatch("resize", {});
    await new Promise<void>((resolve) => setTimeout(resolve, 1));
    assert.deepEqual(resizeRecords(control).map(resizeSize), [
      [1600, 900],
      [1280, 720],
      [1440, 810],
    ]);
  } finally {
    await session.dispose();
  }
});

test("resize observer burst sends its final viewport without input ownership", async () => {
  installBrowser();
  const observers = installResizeObserver();
  const sockets: FakeWebSocket[] = [];
  const canvas = new FakeTarget();
  const session = new WaywireSession({
    endpoint: "https://desktop.example.com",
    remoteDisplay: { mode: "observe", debounceMs: 100 },
    createWebSocket() {
      const created = new FakeWebSocket();
      sockets.push(created);
      return socket(created);
    },
  });
  session.attachSurface(surfaceOptions(canvas));
  try {
    session.connect();
    await flush();
    const control = sockets[0];
    if (!control) throw new Error("control socket was not created");
    control.readyState = FakeWebSocket.OPEN;
    control.dispatch("open", {});
    const observer = observers[0];
    if (!observer) throw new Error("resize observer was not created");
    observer.trigger();

    for (const [width, height, delay] of [
      [1200, 700, 0],
      [1840, 900, 120],
      [1180, 690, 120],
      [1840, 900, 120],
    ] as const) {
      await new Promise<void>((resolve) => setTimeout(resolve, delay));
      canvas.rect.width = width;
      canvas.rect.height = height;
      observer.trigger();
    }
    await new Promise<void>((resolve) => setTimeout(resolve, 110));

    const records = resizeRecords(control);
    assert.deepEqual(records.slice(-4).map(resizeSize), [
      [1200, 700],
      [1840, 900],
      [1180, 690],
      [1840, 900],
    ]);
  } finally {
    await session.dispose();
  }
});

test("reset restore presents an external resize on its later matching generation", async () => {
  installBrowser();
  const installed = installQueueVideoDecoder();
  const sockets = new Map<string, FakeWebSocket>();
  const resizeEvents: ResizeEvent[] = [];
  const canvas = new FakeTarget();
  canvas.getContext = () => ({ drawImage() {} });
  const session = new WaywireSession({
    endpoint: "https://desktop.example.com",
    remoteDisplay: {
      mode: "fixed",
      width: 1600,
      height: 900,
      scale: 1,
    },
    createWebSocket(path) {
      const created = new FakeWebSocket();
      sockets.set(path, created);
      return socket(created);
    },
  });
  session.attachSurface(surfaceOptions(canvas));
  session.on("resize", (event) => resizeEvents.push(event));
  try {
    session.input.acquire();
    session.connect();
    await flush();
    const control = sockets.get("/control");
    const video = sockets.get("/stream");
    if (!control || !video) throw new Error("session sockets were not created");
    control.readyState = FakeWebSocket.OPEN;
    control.dispatch("open", {});
    control.dispatch("message", {
      data: JSON.stringify({ type: "control-state", state: "active" }),
    });
    const resize = resizeRecords(control)[0];
    if (!resize) throw new Error("fixed resize was not sent");
    const [appliedWidth, appliedHeight] = resizeSize(resize);

    session.video.reset();
    control.dispatch("message", {
      data: JSON.stringify({
        type: "resize-applied",
        request: resizeRequestID(resize),
        width: appliedWidth,
        height: appliedHeight,
        scale: 120,
        generation: 2,
      }),
    });
    configureVideo(video);
    await new Promise<void>((resolve) => setImmediate(resolve));
    video.dispatch("message", {
      data: videoPacket(4_000, {
        keyframe: true,
        generation: 4,
        width: appliedWidth,
        height: appliedHeight,
      }),
    });
    await flush();

    assert.deepEqual(
      resizeEvents.map((event) => event.state),
      ["requested", "applied"],
    );
    installed.decoder()?.outputAll();
    await flush();
    assert.deepEqual(
      resizeEvents.map((event) => event.state),
      ["requested", "applied", "presented"],
    );
    assert.deepEqual(resizeEvents.at(-1), {
      state: "presented",
      latencyMs: resizeEvents.at(-1)?.latencyMs,
      width: appliedWidth,
      height: appliedHeight,
      generation: 4,
    });
  } finally {
    await session.dispose();
  }
});

test("75 and 50 percent video settle applied output resizes as presented", async () => {
  installBrowser();
  const installed = installQueueVideoDecoder();
  const sockets = new Map<string, FakeWebSocket>();
  const resizeEvents: ResizeEvent[] = [];
  const canvas = new FakeTarget();
  canvas.getContext = () => ({ drawImage() {} });
  const session = new WaywireSession({
    endpoint: "https://desktop.example.com",
    statsIntervalMs: 0,
    remoteDisplay: { mode: "fixed", width: 1600, height: 900, scale: 1 },
    createWebSocket(path) {
      const created = new FakeWebSocket();
      sockets.set(path, created);
      return socket(created);
    },
  });
  session.attachSurface(surfaceOptions(canvas));
  session.on("resize", (event) => resizeEvents.push(event));
  try {
    session.input.acquire();
    session.connect();
    await flush();
    const control = sockets.get("/control");
    const video = sockets.get("/stream");
    if (!control || !video) throw new Error("session sockets were not created");
    control.readyState = FakeWebSocket.OPEN;
    control.dispatch("open", {});
    control.dispatch("message", {
      data: JSON.stringify({ type: "control-state", state: "active" }),
    });
    configureVideo(video);
    await new Promise<void>((resolve) => setImmediate(resolve));

    const first = resizeRecords(control)[0];
    if (!first) throw new Error("first fixed resize was not sent");
    control.dispatch("message", {
      data: JSON.stringify({
        type: "resize-applied",
        request: resizeRequestID(first),
        width: 1600,
        height: 900,
        scale: 120,
        generation: 2,
      }),
    });
    video.dispatch("message", {
      data: videoPacket(2_000, {
        keyframe: true,
        generation: 2,
        width: 1200,
        height: 674,
      }),
    });
    await flush();
    assert.equal(resizeEvents.at(-1)?.state, "applied");
    installed.decoder()?.outputAll();
    await flush();
    assert.equal(resizeEvents.at(-1)?.state, "presented");
    assert.equal(resizeEvents.at(-1)?.width, 1200);
    assert.equal(resizeEvents.at(-1)?.height, 674);

    session.remoteDisplay.fixed({ width: 1280, height: 720, scale: 1 });
    const second = resizeRecords(control)[1];
    if (!second) throw new Error("second fixed resize was not sent");
    control.dispatch("message", {
      data: JSON.stringify({
        type: "resize-applied",
        request: resizeRequestID(second),
        width: 1280,
        height: 720,
        scale: 120,
        generation: 3,
      }),
    });
    video.dispatch("message", {
      data: videoPacket(3_000, {
        keyframe: true,
        generation: 3,
        width: 640,
        height: 360,
      }),
    });
    await flush();
    assert.equal(resizeEvents.at(-1)?.state, "applied");
    installed.decoder()?.outputAll();
    await flush();
    assert.equal(resizeEvents.at(-1)?.width, 640);
    assert.equal(resizeEvents.at(-1)?.height, 360);

    assert.deepEqual(
      resizeEvents.map((event) => event.state),
      [
        "requested",
        "applied",
        "presented",
        "requested",
        "applied",
        "presented",
      ],
    );
    assert.equal(session.stats.resizeState, "presented");
  } finally {
    await session.dispose();
  }
});

test("video reset sends the input-owner control command", async () => {
  installBrowser();
  const sockets = new Map<string, FakeWebSocket>();
  const session = new WaywireSession({
    endpoint: "https://desktop.example.com",
    createWebSocket(path) {
      const created = new FakeWebSocket();
      sockets.set(path, created);
      return socket(created);
    },
  });
  session.attachSurface(surfaceOptions());
  const errors: Error[] = [];
  session.on("error", (error) => errors.push(error));
  try {
    session.input.acquire();
    session.connect();
    await flush();
    const control = sockets.get("/control");
    if (!control) throw new Error("control socket was not created");
    control.readyState = FakeWebSocket.OPEN;
    control.dispatch("open", {});
    control.dispatch("message", {
      data: JSON.stringify({ type: "control-state", state: "active" }),
    });
    session.video.reset();
    assert.ok(control.sent.includes(JSON.stringify({ type: "reset-video" })));
    control.dispatch("message", {
      data: JSON.stringify({
        type: "reset-video-refused",
        reason: "current-mode-unknown",
      }),
    });
    assert.equal(
      errors.at(-1)?.message,
      "Video reset refused: current-mode-unknown",
    );
  } finally {
    await session.dispose();
  }
});

test("reconnect sends one unchanged resize across observer and ownership callbacks", async () => {
  installBrowser();
  const observers = installResizeObserver();
  const controls: FakeWebSocket[] = [];
  const session = new WaywireSession({
    endpoint: "https://desktop.example.com",
    remoteDisplay: { mode: "observe", debounceMs: 10 },
    createWebSocket(path) {
      const created = new FakeWebSocket();
      if (path === "/control") controls.push(created);
      return socket(created);
    },
  });
  session.attachSurface(surfaceOptions());
  try {
    session.input.acquire();
    session.connect();
    await flush();
    const first = controls[0];
    if (!first) throw new Error("first control socket was not created");
    first.readyState = FakeWebSocket.OPEN;
    first.dispatch("open", {});
    first.dispatch("message", {
      data: JSON.stringify({ type: "control-state", state: "active" }),
    });
    await new Promise<void>((resolve) => setTimeout(resolve, 15));
    assert.equal(resizeRecords(first).length, 1);

    session.disconnect();
    session.connect();
    await flush();
    session.input.acquire();
    const second = controls[1];
    if (!second)
      throw new Error("second session control socket was not created");
    second.readyState = FakeWebSocket.OPEN;
    second.dispatch("open", {});
    await new Promise<void>((resolve) => setTimeout(resolve, 15));
    assert.equal(observers.length, 2);
    assert.equal(resizeRecords(second).length, 0);

    second.dispatch("message", {
      data: JSON.stringify({ type: "control-state", state: "active" }),
    });
    await new Promise<void>((resolve) => setTimeout(resolve, 15));
    assert.equal(resizeRecords(second).length, 1);
  } finally {
    await session.dispose();
  }
});

test("a native resolution selection sends scale 120 after releasing ownership without moving focus", async () => {
  const { document } = installBrowser();
  const control = new FakeWebSocket();
  const canvas = new FakeTarget();
  const select = new FakeTarget();
  const session = new WaywireSession({
    endpoint: "https://remote.example.com",
    createWebSocket: (path) =>
      socket(path === "/control" ? control : new FakeWebSocket()),
  });
  session.attachSurface({ ...surfaceOptions(canvas), controlOnFocus: true });
  try {
    session.connect();
    await flush();
    control.readyState = FakeWebSocket.OPEN;
    control.dispatch("open", {});
    session.input.acquire();
    control.dispatch("message", {
      data: JSON.stringify({ type: "control-state", state: "active" }),
    });
    document.activeElement = select;
    canvas.dispatch("blur", { relatedTarget: select });
    const before = resizeRecords(control).length;
    // These are the viewer's selection-change actions, in their actual order.
    const acquireMessagesBefore = control.sent.filter(
      (value) => value === JSON.stringify({ type: "acquire" }),
    ).length;
    session.remoteDisplay.setPolicy({
      mode: "fixed",
      width: 1366,
      height: 768,
      scale: 1,
    });
    const resized = resizeRecords(control).at(-1);
    assert.ok(resized);
    assert.equal(resizeRecords(control).length, before + 1);
    assert.equal(
      control.sent.filter(
        (value) => value === JSON.stringify({ type: "acquire" }),
      ).length,
      acquireMessagesBefore,
    );
    assert.deepEqual(resizeSize(resized), [1366, 768]);
    assert.equal(new DataView(resized).getUint16(16, true), 120);
    assert.equal(document.activeElement, select);
  } finally {
    await session.dispose();
  }
});
