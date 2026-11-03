// Run instrument-worker.js in waywire's video worker, the way agent-browser's
// --init-script does for pages, which it cannot do for workers: it is
// evaluated once the worker has a context. That can come after the worker's
// own top level, so instrument.js holds the viewer's messages to the worker
// until the instrument says hello.
// Stays attached until killed.
//
// usage: node worker_instrument.mjs CDP_BROWSER_URL SCRIPT_PATH
import { readFileSync } from "node:fs";

const [browserUrl, scriptPath] = process.argv.slice(2);
const source = readFileSync(scriptPath, "utf8");
const socket = new WebSocket(browserUrl);
await new Promise((resolve, reject) => {
  socket.onopen = resolve;
  socket.onerror = () => reject(new Error(`cannot connect to ${browserUrl}`));
});
let nextId = 0;
const pending = new Map();
const send = (method, params = {}, sessionId = undefined) =>
  new Promise((resolve, reject) => {
    const id = ++nextId;
    pending.set(id, { resolve, reject });
    socket.send(JSON.stringify({ id, method, params, sessionId }));
  });
const ignore = () => undefined;
const workers = new Set();
const instrumented = new Set();

// A context can be reported before its interfaces exist; the instrument then
// throws before changing anything and is tried again.
const evaluate = async (sessionId, contextId) => {
  for (let attempt = 0; attempt < 20; attempt++) {
    try {
      const result = await send(
        "Runtime.evaluate",
        { expression: source, contextId },
        sessionId,
      );
      if (!result.exceptionDetails) return;
      console.error(
        "worker instrument failed",
        result.exceptionDetails.exception?.description ??
          result.exceptionDetails.text,
      );
    } catch (error) {
      console.error("worker instrument failed", error.message);
      return;
    }
    await new Promise((resolve) => setTimeout(resolve, 100));
  }
};

socket.onmessage = (event) => {
  const message = JSON.parse(event.data);
  const reply = pending.get(message.id);
  if (reply) {
    pending.delete(message.id);
    if (message.error) reply.reject(new Error(JSON.stringify(message.error)));
    else reply.resolve(message.result);
    return;
  }
  const { sessionId } = message;
  if (
    message.method === "Runtime.executionContextCreated" &&
    workers.has(sessionId) &&
    !instrumented.has(sessionId)
  ) {
    instrumented.add(sessionId);
    void evaluate(sessionId, message.params.context.id);
  } else if (message.method === "Target.attachedToTarget") {
    const { sessionId: child, targetInfo } = message.params;
    const run = () =>
      send("Runtime.runIfWaitingForDebugger", {}, child).catch(ignore);
    if (targetInfo.type === "page") {
      send(
        "Target.setAutoAttach",
        { autoAttach: true, waitForDebuggerOnStart: true, flatten: true },
        child,
      )
        .then(run)
        .catch(ignore);
    } else if (
      targetInfo.type === "worker" &&
      targetInfo.url.includes("/video-worker")
    ) {
      workers.add(child);
      // Runtime.enable answers only once the worker runs, and reports its
      // context then.
      run();
      send("Runtime.enable", {}, child).catch(ignore);
    } else {
      run();
    }
  }
};
await send("Target.setAutoAttach", {
  autoAttach: true,
  waitForDebuggerOnStart: false,
  flatten: true,
});
console.log("ready");
setInterval(() => {}, 1 << 30);
