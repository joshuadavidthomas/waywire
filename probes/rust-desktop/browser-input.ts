// CDP supplies complete physical-key sequences that agent-browser's shortcut
// helper does not emit. Use only the private browser launched for this trial.
import assert from "node:assert/strict";
import { parseArgs } from "node:util";

const { values } = parseArgs({
  options: {
    port: { type: "string" },
    action: { type: "string" },
    "from-x": { type: "string" },
    "from-y": { type: "string" },
    "to-x": { type: "string" },
    "to-y": { type: "string" },
    "delta-y": { type: "string" },
    letter: { type: "string" },
  },
  strict: true,
});
const port = Number(values.port);
assert(
  Number.isInteger(port) && port > 0 && port <= 65535,
  "--port is required",
);
assert(
  values.action === "permissions" ||
    values.action === "shift-save" ||
    values.action === "save" ||
    values.action === "letter" ||
    values.action === "paste-replace" ||
    values.action === "drag" ||
    values.action === "move" ||
    values.action === "scroll",
  "--action must be permissions, shift-save, save, letter, paste-replace, drag, move, or scroll",
);

function coordinate(name: "from-x" | "from-y" | "to-x" | "to-y"): number {
  const value = Number(values[name]);
  assert(Number.isFinite(value) && value >= 0, `--${name} is required`);
  return value;
}
const origin = "http://127.0.0.1:3217";
const pages = (await fetch(`http://127.0.0.1:${port}/json/list`, {
  signal: AbortSignal.timeout(5000),
}).then((response) => response.json())) as Array<{
  id: string;
  type: string;
  url: string;
  webSocketDebuggerUrl: string;
}>;
const targets = pages.filter(
  (page) => page.type === "page" && page.url === `${origin}/`,
);
assert.equal(targets.length, 1, "expected exactly one private Rust viewer tab");
const version = (await fetch(`http://127.0.0.1:${port}/json/version`, {
  signal: AbortSignal.timeout(5000),
}).then((response) => response.json())) as { webSocketDebuggerUrl: string };
const socket = new WebSocket(
  values.action === "permissions"
    ? version.webSocketDebuggerUrl
    : targets[0]!.webSocketDebuggerUrl,
);
const deadline = setTimeout(() => {
  console.error("CDP input check timed out");
  socket.close();
  process.exitCode = 1;
}, 10_000);
let nextId = 0;
const pending = new Map<
  number,
  { resolve: (value: unknown) => void; reject: (error: Error) => void }
>();
socket.addEventListener("message", (event) => {
  const response = JSON.parse(String(event.data)) as {
    id?: number;
    result?: unknown;
    error?: { message: string };
  };
  if (response.id === undefined) return;
  const request = pending.get(response.id);
  if (!request) return;
  pending.delete(response.id);
  if (response.error) request.reject(new Error(response.error.message));
  else request.resolve(response.result);
});
socket.addEventListener("close", () => {
  for (const request of pending.values())
    request.reject(new Error("CDP connection closed"));
  pending.clear();
});
function call(
  method: string,
  params: Record<string, unknown>,
): Promise<unknown> {
  return new Promise((resolve, reject) => {
    const id = ++nextId;
    pending.set(id, { resolve, reject });
    socket.send(JSON.stringify({ id, method, params }));
  });
}
async function key(
  type: "rawKeyDown" | "keyUp",
  key: string,
  code: string,
  windowsVirtualKeyCode: number,
  modifiers: number,
) {
  await call("Input.dispatchKeyEvent", {
    type,
    key,
    code,
    windowsVirtualKeyCode,
    modifiers,
  });
}
async function shortcut(
  code: string,
  keyValue: string,
  virtualKey: number,
): Promise<void> {
  await key("rawKeyDown", "Control", "ControlLeft", 17, 2);
  await key("rawKeyDown", keyValue, code, virtualKey, 2);
  await key("keyUp", keyValue, code, virtualKey, 2);
  await key("keyUp", "Control", "ControlLeft", 17, 0);
}
try {
  await new Promise<void>((resolve, reject) => {
    socket.addEventListener("open", () => resolve(), { once: true });
    socket.addEventListener(
      "error",
      () => reject(new Error("CDP connection failed")),
      { once: true },
    );
  });
  if (values.action === "permissions") {
    const info = (await call("Target.getTargetInfo", {
      targetId: targets[0]!.id,
    })) as { targetInfo: { browserContextId?: string } };
    const contexts = (await call("Target.getBrowserContexts", {})) as {
      browserContextIds: string[];
    };
    const browserContextId = info.targetInfo.browserContextId;
    // Chrome names the default context in TargetInfo but does not accept that
    // name in Browser commands. Only separately created contexts take an ID.
    const context =
      browserContextId && contexts.browserContextIds.includes(browserContextId)
        ? { browserContextId }
        : {};
    await call("Browser.grantPermissions", {
      origin,
      ...context,
      permissions: ["clipboardReadWrite", "clipboardSanitizedWrite"],
    });
    // Permission overrides belong to this DevTools connection. Keep it alive
    // during the check; disconnecting restores the browser's ordinary policy.
    clearTimeout(deadline);
    console.log(
      `Clipboard permission scope PID ${process.pid}; terminate after the check`,
    );
    await new Promise<void>((resolve) => {
      process.once("SIGTERM", resolve);
      process.once("SIGINT", resolve);
    });
  } else if (values.action === "shift-save") {
    await key("rawKeyDown", "Shift", "ShiftLeft", 16, 8);
    await key("rawKeyDown", "R", "KeyR", 82, 8);
    await key("keyUp", "R", "KeyR", 82, 8);
    await key("keyUp", "Shift", "ShiftLeft", 16, 0);
    await shortcut("KeyS", "s", 83);
  } else if (values.action === "save") {
    await shortcut("KeyS", "s", 83);
  } else if (values.action === "letter") {
    assert(
      values.letter?.length === 1 && /^[a-z]$/u.test(values.letter),
      "--letter must be one lowercase ASCII letter",
    );
    const upper = values.letter.toUpperCase();
    const virtualKey = upper.charCodeAt(0);
    await key("rawKeyDown", values.letter, `Key${upper}`, virtualKey, 0);
    await key("keyUp", values.letter, `Key${upper}`, virtualKey, 0);
  } else if (values.action === "paste-replace") {
    await shortcut("KeyA", "a", 65);
    await shortcut("KeyV", "v", 86);
  } else if (values.action === "drag") {
    const from = { x: coordinate("from-x"), y: coordinate("from-y") };
    const to = { x: coordinate("to-x"), y: coordinate("to-y") };
    await call("Input.dispatchMouseEvent", {
      type: "mouseMoved",
      ...from,
      button: "none",
      buttons: 0,
    });
    await call("Input.dispatchMouseEvent", {
      type: "mousePressed",
      ...from,
      button: "left",
      buttons: 1,
      clickCount: 1,
    });
    for (let step = 1; step <= 8; step += 1) {
      await call("Input.dispatchMouseEvent", {
        type: "mouseMoved",
        x: from.x + ((to.x - from.x) * step) / 8,
        y: from.y + ((to.y - from.y) * step) / 8,
        button: "left",
        buttons: 1,
      });
      await new Promise((resolve) => setTimeout(resolve, 20));
    }
    await call("Input.dispatchMouseEvent", {
      type: "mouseReleased",
      ...to,
      button: "left",
      buttons: 0,
      clickCount: 1,
    });
  } else if (values.action === "move") {
    await call("Input.dispatchMouseEvent", {
      type: "mouseMoved",
      x: coordinate("from-x"),
      y: coordinate("from-y"),
      button: "none",
      buttons: 0,
    });
  } else {
    const x = coordinate("from-x");
    const y = coordinate("from-y");
    const deltaY = Number(values["delta-y"]);
    assert(Number.isFinite(deltaY) && deltaY !== 0, "--delta-y is required");
    await call("Input.dispatchMouseEvent", {
      type: "mouseMoved",
      x,
      y,
      button: "none",
      buttons: 0,
    });
    await call("Input.dispatchMouseEvent", {
      type: "mouseWheel",
      x,
      y,
      deltaX: 0,
      deltaY,
      button: "none",
      buttons: 0,
    });
  }
  console.log(
    `${values.action} delivered; verify the native result separately`,
  );
} finally {
  clearTimeout(deadline);
  socket.close();
}
