import assert from "node:assert/strict";
import test from "node:test";

import { WaywireSession } from "./session.ts";
import {
  FakeTarget,
  FakeWebSocket,
  flush,
  installBrowser,
  installGlobal,
  socket,
  surfaceOptions,
} from "./test-support.ts";

const acquire = JSON.stringify({ type: "acquire" });

function reportOwnership(
  control: FakeWebSocket,
  state: "active" | "busy" | "ready",
): void {
  control.dispatch("message", {
    data: JSON.stringify({ type: "control-state", state }),
  });
}

function requestCount(control: FakeWebSocket): number {
  return control.sent.filter((message) => message === acquire).length;
}

function interaction(): object {
  return {
    clientX: 20,
    clientY: 20,
    button: 0,
    pointerId: 1,
    code: "KeyA",
    key: "a",
    repeat: false,
    deltaX: 0,
    deltaY: 1,
    deltaMode: 0,
    preventDefault() {},
    stopPropagation() {},
  };
}

async function ownershipFixture(mode: "automatic" | "manual" = "automatic") {
  const browser = installBrowser();
  installGlobal("WheelEvent", { DOM_DELTA_LINE: 1, DOM_DELTA_PAGE: 2 });
  const controls: FakeWebSocket[] = [];
  const canvas = new FakeTarget();
  const ime = new FakeTarget();
  canvas.focus = () => {
    Object.assign(browser.document, { activeElement: canvas });
    canvas.dispatch("focus", {});
  };
  canvas.addEventListener("blur", () => {
    Object.assign(browser.document, { activeElement: null });
  });
  const session = new WaywireSession({
    endpoint: "https://remote.example.com",
    createWebSocket(path) {
      const created = new FakeWebSocket();
      if (path === "/control") controls.push(created);
      return socket(created);
    },
  });
  session.attachSurface({
    ...surfaceOptions(canvas, ime),
    controlOnFocus: mode === "automatic",
  });
  session.connect();
  await flush();
  const control = controls[0];
  assert.ok(control);
  control.readyState = FakeWebSocket.OPEN;
  control.dispatch("open", {});
  reportOwnership(control, "ready");
  return { ...browser, canvas, ime, session, control, controls };
}

test("focus acquires available input ownership without a click", async () => {
  const { canvas, session, control } = await ownershipFixture();
  try {
    assert.equal(requestCount(control), 0);
    canvas.focus();
    assert.equal(requestCount(control), 1);
    canvas.focus();
    assert.equal(
      requestCount(control),
      1,
      "focus must not duplicate a pending request",
    );
    reportOwnership(control, "active");
    assert.equal(session.state.input.state, "active");
    canvas.dispatch("blur", { relatedTarget: null });
    canvas.focus();
    assert.equal(
      requestCount(control),
      2,
      "normal focus acquisition remains seamless",
    );
  } finally {
    await session.dispose();
  }
});

test("busy ownership waits without timers and reacquires once when ready and focused", async (t) => {
  t.mock.timers.enable({ apis: ["setTimeout", "setInterval"] });
  const { canvas, document, session, control } = await ownershipFixture();
  try {
    canvas.focus();
    reportOwnership(control, "busy");
    t.mock.timers.tick(60_000);
    await flush();
    assert.equal(requestCount(control), 1);
    assert.equal(session.state.input.state, "busy");
    canvas.dispatch("pointerenter", interaction());
    canvas.dispatch("pointermove", interaction());
    canvas.dispatch("blur", { relatedTarget: null });
    canvas.focus();
    document.hidden = true;
    document.dispatch("visibilitychange", {});
    document.hidden = false;
    Object.assign(document, { activeElement: canvas });
    document.dispatch("visibilitychange", {});
    await flush();
    assert.equal(requestCount(control), 1);

    reportOwnership(control, "ready");
    assert.equal(
      requestCount(control),
      2,
      "focused presence can acquire again",
    );
    reportOwnership(control, "ready");
    assert.equal(
      requestCount(control),
      2,
      "readiness must not duplicate a pending request",
    );
  } finally {
    await session.dispose();
  }
});

test("readiness while away clears waiting so the next hover can acquire", async (t) => {
  t.mock.timers.enable({ apis: ["setTimeout", "setInterval"] });
  const { canvas, document, session, control } = await ownershipFixture();
  try {
    canvas.focus();
    canvas.dispatch("blur", { relatedTarget: null });
    reportOwnership(control, "busy");
    reportOwnership(control, "ready");
    t.mock.timers.tick(10_000);
    await flush();
    assert.equal(
      requestCount(control),
      1,
      "readiness must not claim for an absent viewer",
    );
    assert.equal(document.activeElement, null);
    canvas.focus();
    assert.equal(
      requestCount(control),
      2,
      "hover focus still acquires after contention",
    );
  } finally {
    await session.dispose();
  }
});

for (const absence of ["hidden", "unfocused"] as const) {
  test(`readiness does not acquire while the page is ${absence}`, async () => {
    const { canvas, document, session, control } = await ownershipFixture();
    try {
      canvas.focus();
      reportOwnership(control, "busy");
      if (absence === "hidden") document.hidden = true;
      else document.hasFocus = () => false;
      reportOwnership(control, "ready");
      assert.equal(requestCount(control), 1);
    } finally {
      await session.dispose();
    }
  });
}

for (const returnEvent of ["window focus", "pointerenter"] as const) {
  test(`returning by ${returnEvent} acquires after readiness without a new canvas focus event`, async () => {
    const { canvas, document, window, session, control } =
      await ownershipFixture();
    try {
      canvas.focus();
      reportOwnership(control, "busy");
      document.hasFocus = () => false;
      reportOwnership(control, "ready");
      assert.equal(requestCount(control), 1);
      document.hasFocus = () => true;
      if (returnEvent === "window focus") window.dispatch("focus", {});
      else canvas.dispatch("pointerenter", interaction());
      assert.equal(requestCount(control), 2);
      canvas.dispatch("pointerenter", interaction());
      window.dispatch("focus", {});
      assert.equal(requestCount(control), 2);
    } finally {
      await session.dispose();
    }
  });
}

test("readiness respects a surface with automatic acquisition disabled", async () => {
  const { canvas, session, control } = await ownershipFixture("manual");
  try {
    canvas.focus();
    assert.equal(requestCount(control), 0);
    session.input.acquire();
    reportOwnership(control, "busy");
    reportOwnership(control, "ready");
    assert.equal(requestCount(control), 1);
    session.input.acquire();
    assert.equal(requestCount(control), 2);
  } finally {
    await session.dispose();
  }
});

test("busy ownership retries once per intentional act, never in the background", async (t) => {
  t.mock.timers.enable({ apis: ["setTimeout", "setInterval"] });
  const { canvas, ime, session, control } = await ownershipFixture();
  try {
    canvas.focus();
    let requests = 1;
    for (const type of ["pointerdown", "keydown", "wheel", "touchstart"]) {
      reportOwnership(control, "busy");
      canvas.dispatch(type, interaction());
      requests += 1;
      assert.equal(requestCount(control), requests, type);
      // A touch may also produce a pointer event; focus can fire in the same act.
      canvas.dispatch("touchstart", interaction());
      canvas.focus();
      assert.equal(requestCount(control), requests);
      reportOwnership(control, "busy");
      t.mock.timers.tick(10_000);
      await flush();
      assert.equal(requestCount(control), requests);
    }
    canvas.dispatch("keydown", { ...interaction(), repeat: true });
    assert.equal(
      requestCount(control),
      requests,
      "key repeat is not a new act",
    );
    ime.dispatch("keydown", interaction());
    requests += 1;
    assert.equal(requestCount(control), requests);
    reportOwnership(control, "busy");
    session.input.acquire();
    requests += 1;
    assert.equal(
      requestCount(control),
      requests,
      "the Remote input switch can retry",
    );
    reportOwnership(control, "active");
    assert.equal(session.state.input.state, "active");
  } finally {
    await session.dispose();
  }
});

test("turning input off blocks focus and intentional retries until it is enabled", async () => {
  const { canvas, ime, session, control } = await ownershipFixture();
  try {
    canvas.focus();
    reportOwnership(control, "busy");
    session.input.release();
    canvas.dispatch("blur", { relatedTarget: null });
    canvas.focus();
    reportOwnership(control, "ready");
    ime.dispatch("focus", {});
    for (const type of ["pointerdown", "keydown", "wheel", "touchstart"])
      canvas.dispatch(type, interaction());
    assert.equal(requestCount(control), 1);
    session.input.acquire();
    assert.equal(requestCount(control), 2);
    reportOwnership(control, "active");
    session.input.release();
    canvas.dispatch("blur", { relatedTarget: null });
    canvas.focus();
    assert.equal(
      requestCount(control),
      2,
      "off also blocks ordinary focus acquisition",
    );
  } finally {
    await session.dispose();
  }
});

test("a focused viewer can acquire on readiness after reconnect", async (t) => {
  t.mock.timers.enable({ apis: ["setTimeout", "setInterval"] });
  const { canvas, session, control, controls } = await ownershipFixture();
  const originalWarn = console.warn;
  console.warn = () => undefined;
  try {
    canvas.focus();
    reportOwnership(control, "busy");
    control.readyState = FakeWebSocket.CLOSED;
    control.dispatch("close", { code: 1006 });
    t.mock.timers.tick(250);
    await flush();
    const reconnected = controls[1];
    assert.ok(reconnected);
    reconnected.readyState = FakeWebSocket.OPEN;
    reconnected.dispatch("open", {});
    assert.equal(requestCount(reconnected), 0);
    reportOwnership(reconnected, "ready");
    assert.equal(requestCount(reconnected), 1);
    canvas.focus();
    assert.equal(requestCount(reconnected), 1);
  } finally {
    console.warn = originalWarn;
    await session.dispose();
  }
});
