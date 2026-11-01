import assert from "node:assert/strict";
import test from "node:test";

import maximumProtocolPixels from "../../../../protocol/max-display-pixels.json";
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

test("default manual policy never observes remote sizing or resizes on connect and acquire", async () => {
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
    assert.equal(observers.length, 1, "the cursor observes canvas layout only");
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
    assert.equal(observers.length, 1, "manual sizing adds no observer");
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
    assert.equal(observers.length, 1, "the cursor observes canvas layout only");

    session.remoteDisplay.observe({
      element: canvas as unknown as Element,
      devicePixelRatio: 1,
      debounceMs: 0,
    });
    await flush();
    assert.equal(observers.length, 2);
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
    const observer = observers[1];
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
    assert.equal(observers.length, 3);
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

test("every resize record the viewer sends fits the Rust FrameSize and ScaleV120 ranges", async () => {
  installBrowser();
  const control = new FakeWebSocket();
  const session = new WaywireSession({
    endpoint: "https://remote.example.com",
    createWebSocket: (path) =>
      socket(path === "/control" ? control : new FakeWebSocket()),
  });
  session.attachSurface(surfaceOptions());
  // A fixed seed keeps failures reproducible.
  let seed = 0x5eed;
  const random = (): number => {
    seed = (seed * 1_103_515_245 + 12_345) % 2 ** 31;
    return seed / 2 ** 31;
  };
  const edges = [
    0.25, 1, 179, 180.5, 319, 320, 321, 333.3, 2161, 5999, 6001, 9999.9,
  ];
  const scales = [0.001, 0.5, 1, 1.004, 1.25, 1.5, 3.999, 4, 4.01, 7];
  const policies: Array<[number, number, number]> = [];
  for (const width of edges)
    for (const height of edges)
      for (const scale of scales) policies.push([width, height, scale]);
  for (let index = 0; index < 4000; index += 1)
    policies.push([
      0.01 + random() * 12_000,
      0.01 + random() * 12_000,
      0.001 + random() * 8,
    ]);
  try {
    session.connect();
    await flush();
    control.readyState = FakeWebSocket.OPEN;
    control.dispatch("open", {});
    for (const [width, height, scale] of policies)
      session.remoteDisplay.setPolicy({ mode: "fixed", width, height, scale });
    const records = resizeRecords(control);
    assert.ok(
      records.length > policies.length / 2,
      `${records.length} records`,
    );
    for (const record of records) {
      const view = new DataView(record);
      // The Rust Resize layout: header, u32 width, u32 height, u16 scale,
      // u16 request.
      assert.deepEqual(
        [...new Uint8Array(record, 0, 8)],
        [PROTOCOL_VERSION, 6, 0, 0, 12, 0, 0, 0],
      );
      assert.equal(record.byteLength, 20);
      const width = view.getUint32(8, true);
      const height = view.getUint32(12, true);
      const scale = view.getUint16(16, true);
      const accepted =
        width >= 320 &&
        width <= 6000 &&
        width % 2 === 0 &&
        height >= 180 &&
        height <= 6000 &&
        height % 2 === 0 &&
        width * height <= maximumProtocolPixels &&
        scale >= 120 &&
        scale <= 480 &&
        view.getUint16(18, true) !== 0;
      assert.ok(
        accepted,
        `${width}x${height}@${scale} is outside the protocol`,
      );
    }
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

function keyEvent(options: {
  code: string;
  key?: string;
  isComposing?: boolean;
  metaKey?: boolean;
  ctrlKey?: boolean;
  shiftKey?: boolean;
}): object {
  return {
    code: options.code,
    key: options.key ?? "a",
    repeat: false,
    isComposing: options.isComposing ?? false,
    keyCode: 0,
    ctrlKey: options.ctrlKey ?? false,
    metaKey: options.metaKey ?? false,
    shiftKey: options.shiftKey ?? false,
    altKey: false,
    preventDefault() {},
    stopPropagation() {},
  };
}

function keyboardRecords(
  control: FakeWebSocket,
): Array<{ key: number; state: number }> {
  return control.sent
    .filter(
      (value): value is ArrayBuffer =>
        value instanceof ArrayBuffer && new DataView(value).getUint8(1) === 4,
    )
    .map((record) => {
      const view = new DataView(record);
      return { key: view.getUint32(8, true), state: view.getUint8(12) };
    });
}

async function keyboardFixture(
  options: { clipboardAutoSync?: boolean } = {},
): Promise<{
  canvas: FakeTarget;
  ime: FakeTarget;
  document: FakeTarget;
  session: WaywireSession;
  control: FakeWebSocket;
}> {
  const browser = installBrowser();
  installGlobal("WheelEvent", { DOM_DELTA_LINE: 1, DOM_DELTA_PAGE: 2 });
  const control = new FakeWebSocket();
  const canvas = new FakeTarget();
  const ime = new FakeTarget();
  const session = new WaywireSession({
    endpoint: "https://desktop.example.com",
    createWebSocket: (path) =>
      socket(path === "/control" ? control : new FakeWebSocket()),
  });
  session.attachSurface({
    ...surfaceOptions(canvas, ime),
    ...(options.clipboardAutoSync ? { clipboardAutoSync: true } : {}),
  });
  session.input.acquire();
  session.connect();
  await flush();
  control.readyState = FakeWebSocket.OPEN;
  control.dispatch("open", {});
  control.dispatch("message", {
    data: JSON.stringify({ type: "control-state", state: "active" }),
  });
  return { ...browser, canvas, ime, session, control };
}

test("dead key composition commits text without a physical accent", async () => {
  const { ime, session, control } = await keyboardFixture();
  try {
    ime.dispatch("keydown", keyEvent({ code: "Quote", key: "Dead" }));
    ime.dispatch("keyup", keyEvent({ code: "Quote", key: "Dead" }));
    ime.dispatch(
      "keydown",
      keyEvent({ code: "KeyE", key: "e", isComposing: true }),
    );
    ime.dispatch("compositionend", { data: "é" });
    ime.dispatch("beforeinput", {
      inputType: "insertText",
      data: "é",
      isComposing: false,
      preventDefault() {},
    });
    assert.deepEqual(keyboardRecords(control), []);
    assert.deepEqual(
      control.sent
        .filter(
          (value) =>
            typeof value === "string" && value.includes('"type":"text"'),
        )
        .map((value) => JSON.parse(String(value))),
      [{ type: "text", action: "commit", text: "é", sequence: 1 }],
    );
  } finally {
    await session.dispose();
  }
});

test("Japanese and Korean keys reach the remote as their evdev codes", async () => {
  const { canvas, session, control } = await keyboardFixture();
  // KeyboardEvent code, key value, and the code from linux/input-event-codes.h.
  const keys: Array<[string, string, number]> = [
    ["Lang1", "HangulMode", 122], // KEY_HANGEUL
    ["Lang2", "HanjaMode", 123], // KEY_HANJA
    ["Lang3", "Katakana", 90], // KEY_KATAKANA
    ["Lang4", "Hiragana", 91], // KEY_HIRAGANA
    ["Lang5", "ZenkakuHankaku", 85], // KEY_ZENKAKUHANKAKU
    ["Convert", "Convert", 92], // KEY_HENKAN
    ["NonConvert", "NonConvert", 94], // KEY_MUHENKAN
    ["KanaMode", "KanaMode", 93], // KEY_KATAKANAHIRAGANA
    ["IntlRo", "\\", 89], // KEY_RO
    ["IntlYen", "\u00a5", 124], // KEY_YEN
    ["NumpadComma", ",", 121], // KEY_KPCOMMA
  ];
  try {
    for (const [code, key] of keys) {
      canvas.dispatch("keydown", keyEvent({ code, key }));
      canvas.dispatch("keyup", keyEvent({ code, key }));
    }
    assert.deepEqual(
      keyboardRecords(control),
      keys.flatMap(([, , key]) => [
        { key, state: 1 },
        { key, state: 0 },
      ]),
    );
  } finally {
    await session.dispose();
  }
});

test("releasing Meta releases keys whose keyup macOS never delivered", async () => {
  const { canvas, session, control } = await keyboardFixture();
  installGlobal("navigator", { platform: "MacIntel" });
  try {
    canvas.dispatch("keydown", keyEvent({ code: "KeyA" }));
    canvas.dispatch("keydown", keyEvent({ code: "MetaLeft" }));
    canvas.dispatch("keydown", keyEvent({ code: "KeyB", metaKey: true }));
    assert.deepEqual(keyboardRecords(control), [
      { key: 30, state: 1 },
      { key: 125, state: 1 },
      { key: 48, state: 1 },
    ]);

    canvas.dispatch("keyup", keyEvent({ code: "MetaLeft" }));
    assert.deepEqual(keyboardRecords(control), [
      { key: 30, state: 1 },
      { key: 125, state: 1 },
      { key: 48, state: 1 },
      { key: 125, state: 0 },
      { key: 30, state: 0 },
      { key: 48, state: 0 },
    ]);

    // A late keyup for an already released key sends nothing.
    canvas.dispatch("keyup", keyEvent({ code: "KeyB" }));
    assert.equal(keyboardRecords(control).length, 6);

    // Modifier keyup events may also be lost while Meta is held.
    canvas.dispatch("keydown", keyEvent({ code: "ShiftLeft" }));
    canvas.dispatch("keydown", keyEvent({ code: "KeyB", metaKey: true }));
    canvas.dispatch("keyup", keyEvent({ code: "MetaRight" }));
    assert.deepEqual(keyboardRecords(control).slice(-4), [
      { key: 42, state: 1 },
      { key: 48, state: 1 },
      { key: 42, state: 0 },
      { key: 48, state: 0 },
    ]);
  } finally {
    await session.dispose();
  }
});

test("releasing Meta on other platforms does not release held keys", async () => {
  const { canvas, session, control } = await keyboardFixture();
  installGlobal("navigator", { platform: "Linux x86_64" });
  try {
    canvas.dispatch("keydown", keyEvent({ code: "MetaLeft" }));
    canvas.dispatch("keydown", keyEvent({ code: "KeyC", metaKey: true }));
    canvas.dispatch("keyup", keyEvent({ code: "MetaLeft" }));
    assert.deepEqual(keyboardRecords(control), [
      { key: 125, state: 1 },
      { key: 46, state: 1 },
      { key: 125, state: 0 },
    ]);
    canvas.dispatch("keyup", keyEvent({ code: "KeyC" }));
    assert.deepEqual(keyboardRecords(control).at(-1), { key: 46, state: 0 });
  } finally {
    await session.dispose();
  }
});

test("macOS editing shortcuts use Ctrl while other Cmd shortcuts use Super", async () => {
  const { canvas, session, control } = await keyboardFixture();
  installGlobal("navigator", { platform: "MacIntel" });
  // Select all, cut, copy and undo, with their codes from
  // linux/input-event-codes.h.
  const editing: Array<[string, number]> = [
    ["KeyA", 30],
    ["KeyX", 45],
    ["KeyC", 46],
    ["KeyZ", 44],
  ];
  try {
    canvas.dispatch("keydown", keyEvent({ code: "MetaLeft" }));
    for (const [code] of editing) {
      canvas.dispatch("keydown", keyEvent({ code, metaKey: true }));
      canvas.dispatch("keyup", keyEvent({ code, metaKey: true }));
    }
    canvas.dispatch("keydown", keyEvent({ code: "Tab", metaKey: true }));
    canvas.dispatch("keyup", keyEvent({ code: "Tab", metaKey: true }));
    canvas.dispatch("keyup", keyEvent({ code: "MetaLeft" }));
    assert.deepEqual(keyboardRecords(control), [
      { key: 125, state: 1 },
      ...editing.flatMap(([, key]) => [
        { key: 125, state: 0 },
        { key: 29, state: 1 },
        { key, state: 1 },
        { key, state: 0 },
        { key: 29, state: 0 },
        { key: 125, state: 1 },
      ]),
      { key: 15, state: 1 },
      { key: 15, state: 0 },
      { key: 125, state: 0 },
    ]);
  } finally {
    await session.dispose();
  }
});

test("wheel notches and smooth scroll use distinct records", async () => {
  const { canvas, session, control } = await keyboardFixture();
  const wheel = (deltaY: number, deltaMode: number) => ({
    deltaX: 0,
    deltaY,
    deltaMode,
    preventDefault() {},
  });
  try {
    canvas.dispatch("wheel", wheel(120, 0));
    canvas.dispatch("wheel", wheel(100, 0));
    canvas.dispatch("wheel", wheel(-3, 1));
    canvas.dispatch("wheel", wheel(3, 0));
    await new Promise((resolve) => setTimeout(resolve, 160));
    const records = control.sent.filter(
      (value): value is ArrayBuffer => value instanceof ArrayBuffer,
    );
    assert.deepEqual(
      records.map((record) => new DataView(record).getUint8(1)),
      [14, 14, 14, 3, 15],
    );
    assert.deepEqual(
      records.map((record) => new DataView(record).getFloat32(12, true)),
      [120, 120, -120, 3, 1],
    );
  } finally {
    await session.dispose();
  }
});

test("Cmd+Shift+C taps Ctrl instead of Super and lifts the held Meta", async () => {
  const { canvas, session, control } = await keyboardFixture();
  try {
    canvas.dispatch("keydown", keyEvent({ code: "MetaLeft" }));
    canvas.dispatch(
      "keydown",
      keyEvent({ code: "KeyC", metaKey: true, shiftKey: true }),
    );
    assert.deepEqual(keyboardRecords(control), [
      { key: 125, state: 1 },
      { key: 125, state: 0 },
      { key: 42, state: 1 },
      { key: 29, state: 1 },
      { key: 46, state: 1 },
      { key: 46, state: 0 },
      { key: 42, state: 0 },
      { key: 29, state: 0 },
      { key: 125, state: 1 },
    ]);
  } finally {
    await session.dispose();
  }
});

test("Cmd+V taps Ctrl instead of Super after sending the local clipboard", async () => {
  const { canvas, session, control } = await keyboardFixture();
  installGlobal("navigator", {
    platform: "MacIntel",
    clipboard: { readText: () => Promise.resolve("local text") },
  });
  try {
    canvas.dispatch("keydown", keyEvent({ code: "MetaLeft" }));
    canvas.dispatch("keydown", keyEvent({ code: "KeyV", metaKey: true }));
    await flush();
    const management = new Set([
      JSON.stringify({ type: "acquire" }),
      JSON.stringify({ type: "set-quality", preset: "automatic" }),
    ]);
    assert.deepEqual(
      control.sent.filter(
        (value) => typeof value === "string" && !management.has(value),
      ),
      [JSON.stringify({ type: "clipboard-write", text: "local text" })],
    );
    assert.deepEqual(keyboardRecords(control), [
      { key: 125, state: 1 },
      { key: 125, state: 0 },
      { key: 29, state: 1 },
      { key: 47, state: 1 },
      { key: 47, state: 0 },
      { key: 29, state: 0 },
      { key: 125, state: 1 },
    ]);
  } finally {
    await session.dispose();
  }
});

test("a shortcut tap never re-presses a modifier the user already holds", async () => {
  const { canvas, session, control } = await keyboardFixture();
  installGlobal("navigator", {
    clipboard: { readText: () => Promise.resolve("") },
  });
  try {
    canvas.dispatch("keydown", keyEvent({ code: "ControlLeft" }));
    canvas.dispatch("keydown", keyEvent({ code: "KeyV", ctrlKey: true }));
    await flush();
    assert.deepEqual(keyboardRecords(control), [
      { key: 29, state: 1 },
      { key: 47, state: 1 },
      { key: 47, state: 0 },
    ]);
  } finally {
    await session.dispose();
  }
});

test("acquiring control does not auto-write the clipboard snapshot", async () => {
  const { canvas, session, control } = await keyboardFixture({
    clipboardAutoSync: true,
  });
  const writes: string[] = [];
  installGlobal("navigator", {
    clipboard: {
      writeText: (text: string) => {
        writes.push(text);
        return Promise.resolve();
      },
    },
  });
  const statuses: string[] = [];
  session.on("clipboard", (event) => statuses.push(event.status));
  try {
    control.dispatch("message", {
      data: JSON.stringify({ type: "clipboard-snapshot", text: "snapshot" }),
    });
    assert.deepEqual(writes, []);
    assert.equal(session.clipboard.latestRemoteText, "snapshot");
    assert.deepEqual(statuses, ["Remote clipboard ready"]);

    // Only a later change syncs into the local clipboard.
    control.dispatch("message", {
      data: JSON.stringify({ type: "clipboard", text: "changed" }),
    });
    assert.deepEqual(writes, ["changed"]);
    await flush();
    assert.deepEqual(statuses, [
      "Remote clipboard ready",
      "Remote clipboard ready",
      "Clipboard synced",
    ]);
  } finally {
    await session.dispose();
  }
});

test("the first remote copy syncs when acquisition has no snapshot", async () => {
  const { session, control } = await keyboardFixture({
    clipboardAutoSync: true,
  });
  const writes: string[] = [];
  installGlobal("navigator", {
    clipboard: {
      writeText: (text: string) => {
        writes.push(text);
        return Promise.resolve();
      },
    },
  });
  try {
    control.dispatch("message", {
      data: JSON.stringify({ type: "clipboard", text: "first copy" }),
    });
    assert.deepEqual(writes, ["first copy"]);
  } finally {
    await session.dispose();
  }
});

function remoteClipboard(control: FakeWebSocket, text: string): void {
  control.dispatch("message", {
    data: JSON.stringify({ type: "clipboard", text }),
  });
}

function recordClipboardWrites(
  writeText: (text: string) => Promise<void> = () => Promise.resolve(),
): string[] {
  const writes: string[] = [];
  installGlobal("navigator", {
    clipboard: {
      writeText: (text: string) => {
        writes.push(text);
        return writeText(text);
      },
    },
  });
  return writes;
}

test("clipboard auto-sync stays off unless the surface enables it", async () => {
  const { session, control } = await keyboardFixture();
  const writes = recordClipboardWrites();
  const statuses: string[] = [];
  session.on("clipboard", (event) => statuses.push(event.status));
  try {
    remoteClipboard(control, "remote copy");
    await flush();
    assert.deepEqual(writes, []);
    assert.equal(session.clipboard.latestRemoteText, "remote copy");
    assert.deepEqual(statuses, ["Remote clipboard ready"]);
  } finally {
    await session.dispose();
  }
});

test("clipboard auto-sync writes only non-empty copies while the page is visible and focused", async () => {
  const { session, control, document } = await keyboardFixture({
    clipboardAutoSync: true,
  });
  const writes = recordClipboardWrites();
  const statuses: string[] = [];
  session.on("clipboard", (event) => statuses.push(event.status));
  try {
    document.hidden = true;
    remoteClipboard(control, "while hidden");
    document.hidden = false;
    document.hasFocus = () => false;
    remoteClipboard(control, "while unfocused");
    document.hasFocus = () => true;
    remoteClipboard(control, "");
    await flush();
    assert.deepEqual(writes, []);
    assert.equal(session.clipboard.latestRemoteText, "");
    assert.deepEqual(statuses, [
      "Remote clipboard ready",
      "Remote clipboard ready",
      "Remote clipboard empty",
    ]);

    remoteClipboard(control, "visible and focused");
    await flush();
    assert.deepEqual(writes, ["visible and focused"]);
    assert.equal(statuses.at(-1), "Clipboard synced");
  } finally {
    await session.dispose();
  }
});

test("a refused clipboard auto-sync leaves the remote copy ready", async () => {
  const { session, control } = await keyboardFixture({
    clipboardAutoSync: true,
  });
  const writes = recordClipboardWrites(() =>
    Promise.reject(new Error("Document is not focused")),
  );
  const statuses: string[] = [];
  session.on("clipboard", (event) => statuses.push(event.status));
  try {
    remoteClipboard(control, "refused");
    await flush();
    assert.deepEqual(writes, ["refused"]);
    assert.deepEqual(statuses, [
      "Remote clipboard ready",
      "Remote clipboard ready",
    ]);
    assert.equal(session.clipboard.latestRemoteText, "refused");
  } finally {
    await session.dispose();
  }
});

test("Ctrl+Shift+C copies the next remote clipboard through one ClipboardItem", async () => {
  const { canvas, session, control } = await keyboardFixture({
    clipboardAutoSync: true,
  });
  // The browser resolves each item's data when it writes the clipboard.
  const itemWrites: Array<Promise<string>> = [];
  const writes: string[] = [];
  installGlobal(
    "ClipboardItem",
    class {
      constructor(readonly items: Record<string, Promise<Blob>>) {}
    },
  );
  installGlobal("navigator", {
    clipboard: {
      write: (items: Array<{ items: Record<string, Promise<Blob>> }>) => {
        const texts = items.map(async (item) => {
          const blob = await item.items["text/plain"];
          return blob ? blob.text() : "";
        });
        itemWrites.push(...texts);
        return Promise.all(texts).then(() => undefined);
      },
      writeText: (text: string) => {
        writes.push(text);
        return Promise.resolve();
      },
    },
  });
  const statuses: string[] = [];
  session.on("clipboard", (event) => statuses.push(event.status));
  try {
    canvas.dispatch(
      "keydown",
      keyEvent({ code: "KeyC", ctrlKey: true, shiftKey: true }),
    );
    assert.equal(itemWrites.length, 1, "the shortcut reserves one write");
    remoteClipboard(control, "terminal selection");
    assert.deepEqual(await Promise.all(itemWrites), ["terminal selection"]);
    await flush();
    assert.deepEqual(writes, [], "the copy is not written a second time");
    assert.equal(statuses.at(-1), "Remote clipboard copied");

    // The shortcut covers one copy; later copies auto-sync again.
    remoteClipboard(control, "later copy");
    await flush();
    assert.equal(itemWrites.length, 1);
    assert.deepEqual(writes, ["later copy"]);
  } finally {
    installGlobal("ClipboardItem", undefined);
    await session.dispose();
  }
});

test("agent input state from the control socket reaches the session", async () => {
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
  try {
    session.connect();
    await flush();
    const control = sockets.get("/control");
    if (!control) throw new Error("control socket was not created");
    control.readyState = FakeWebSocket.OPEN;
    control.dispatch("open", {});
    control.dispatch("message", {
      data: JSON.stringify({
        type: "agent-input",
        connected: true,
        dropped: true,
      }),
    });
    assert.deepEqual(session.state.agent, { connected: true, dropped: true });
  } finally {
    await session.dispose();
  }
});
