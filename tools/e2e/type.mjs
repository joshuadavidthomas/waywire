// Type text in a page the way a person on a US keyboard does: each character
// is a keydown and keyup with its physical key code, and shifted characters
// are typed while ShiftLeft is held. agent-browser's `press` sends no key
// code for Shift, so this speaks the DevTools protocol to agent-browser's
// Chromium directly, like tools/benchmark/hold_key.mjs.
//
// usage: node type.mjs CDP_BROWSER_URL PAGE_URL_PREFIX TEXT
const [browserUrl, pagePrefix, text] = process.argv.slice(2);
const PLAIN = "`1234567890-=[]\\;',./ ";
const SHIFTED = '~!@#$%^&*()_+{}|:"<>? ';
const CODES = [
  "Backquote",
  ...[1, 2, 3, 4, 5, 6, 7, 8, 9, 0].map((digit) => `Digit${digit}`),
  "Minus",
  "Equal",
  "BracketLeft",
  "BracketRight",
  "Backslash",
  "Semicolon",
  "Quote",
  "Comma",
  "Period",
  "Slash",
  "Space",
];
const KEY_CODES = [
  192, 49, 50, 51, 52, 53, 54, 55, 56, 57, 48, 189, 187, 219, 221, 220, 186,
  222, 188, 190, 191, 32,
];

function key(character) {
  if (/^[a-z]$/i.test(character)) {
    const upper = character.toUpperCase();
    return {
      code: `Key${upper}`,
      keyCode: upper.charCodeAt(0),
      shift: character === upper,
    };
  }
  let index = PLAIN.indexOf(character);
  const shift = index < 0;
  if (shift) index = SHIFTED.indexOf(character);
  if (index < 0)
    throw new Error(`no US key types ${JSON.stringify(character)}`);
  return { code: CODES[index], keyCode: KEY_CODES[index], shift };
}

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
const dispatch = (params) => send("Input.dispatchKeyEvent", params, sessionId);
const SHIFT = { key: "Shift", code: "ShiftLeft", windowsVirtualKeyCode: 16 };
for (const character of text) {
  const { code, keyCode, shift } = key(character);
  const modifiers = shift ? 8 : 0;
  if (shift) await dispatch({ type: "rawKeyDown", ...SHIFT, modifiers });
  const pressed = { key: character, code, windowsVirtualKeyCode: keyCode };
  await dispatch({
    type: "keyDown",
    ...pressed,
    text: character,
    unmodifiedText: character,
    modifiers,
  });
  await dispatch({ type: "keyUp", ...pressed, modifiers });
  if (shift) await dispatch({ type: "keyUp", ...SHIFT });
}
await send("Target.detachFromTarget", { sessionId });
socket.close();
