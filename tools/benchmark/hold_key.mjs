// Hold one letter key in a page the way a browser does when a person holds
// it: a keydown, `repeats` autorepeat keydowns 33 ms apart, then a keyup.
// agent-browser's `press` cannot send autorepeat keydowns, so this speaks
// the DevTools protocol to agent-browser's Chromium directly.
//
// usage: node hold_key.mjs CDP_BROWSER_URL PAGE_URL_PREFIX LETTER REPEATS
const [browserUrl, pagePrefix, letter, repeats] = process.argv.slice(2);
const socket = new WebSocket(browserUrl);
await new Promise((resolve, reject) => {
  socket.onopen = resolve;
  socket.onerror = () => reject(new Error(`cannot connect to ${browserUrl}`));
});
let nextId = 0;
const pending = new Map();
socket.onmessage = (event) => {
  const message = JSON.parse(event.data);
  const reply = pending.get(message.id);
  if (!reply) return;
  pending.delete(message.id);
  if (message.error) reply.reject(new Error(JSON.stringify(message.error)));
  else reply.resolve(message.result);
};
const send = (method, params = {}, sessionId = undefined) =>
  new Promise((resolve, reject) => {
    const id = ++nextId;
    pending.set(id, { resolve, reject });
    socket.send(JSON.stringify({ id, method, params, sessionId }));
  });

const { targetInfos } = await send("Target.getTargets");
const page = targetInfos.find(
  (target) => target.type === "page" && target.url.startsWith(pagePrefix),
);
if (!page) throw new Error(`no page at ${pagePrefix}`);
const { sessionId } = await send("Target.attachToTarget", {
  targetId: page.targetId,
  flatten: true,
});
const key = {
  key: letter,
  code: `Key${letter.toUpperCase()}`,
  windowsVirtualKeyCode: letter.toUpperCase().charCodeAt(0),
};
for (let index = 0; index <= Number(repeats); index++) {
  if (index > 0) await new Promise((resolve) => setTimeout(resolve, 33));
  await send(
    "Input.dispatchKeyEvent",
    {
      type: "keyDown",
      ...key,
      text: letter,
      unmodifiedText: letter,
      autoRepeat: index > 0,
    },
    sessionId,
  );
}
await send("Input.dispatchKeyEvent", { type: "keyUp", ...key }, sessionId);
await send("Target.detachFromTarget", { sessionId });
socket.close();
