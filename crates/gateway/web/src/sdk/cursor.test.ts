import assert from "node:assert/strict";
import test from "node:test";

import { LocalCursor } from "./cursor.ts";
import { parseControlMessage } from "./messages.ts";
import { FakeTarget, installBrowser } from "./test-support.ts";

// The keyword list pinned in crates/protocol/src/pipe.rs, in wire order.
const rustCursorShapes = [
  "default",
  "context-menu",
  "help",
  "pointer",
  "progress",
  "wait",
  "cell",
  "crosshair",
  "text",
  "vertical-text",
  "alias",
  "copy",
  "move",
  "no-drop",
  "not-allowed",
  "grab",
  "grabbing",
  "e-resize",
  "n-resize",
  "ne-resize",
  "nw-resize",
  "s-resize",
  "se-resize",
  "sw-resize",
  "w-resize",
  "ew-resize",
  "ns-resize",
  "nesw-resize",
  "nwse-resize",
  "col-resize",
  "row-resize",
  "all-scroll",
  "zoom-in",
  "zoom-out",
  "dnd-ask",
  "all-resize",
];

function fixture() {
  const { document, window } = installBrowser();
  const element = new FakeTarget();
  const display = new FakeTarget();
  const host = new FakeTarget();
  display.rect = { width: 1000, height: 800, left: 10, top: 20 };
  element.style.cursor = "crosshair";
  element.style.priority = "important";
  const body = (document as unknown as { body: FakeTarget }).body;
  body.append(host);
  host.append(display);
  const cursor = new LocalCursor(
    element as unknown as HTMLElement,
    display as unknown as HTMLCanvasElement,
  );
  const overlay = body.children[1];
  if (!overlay) throw new Error("cursor overlay was not appended");
  const state = {
    type: "cursor" as const,
    visible: true,
    shape: "pointer" as const,
    position: { x: 10, y: 20 },
  };
  return {
    cursor,
    display,
    element,
    overlay,
    state,
    body,
    document,
    host,
    window,
  };
}

function overlayPosition(overlay: FakeTarget): { x: number; y: number } {
  const coordinates = overlay.style.transform.match(
    /^translate3d\(([-\d.]+)px, ([-\d.]+)px, 0\)$/,
  );
  assert.ok(coordinates);
  return { x: Number(coordinates[1]), y: Number(coordinates[2]) };
}

test("cursor parser accepts every shape the Rust protocol can send", () => {
  assert.equal(rustCursorShapes.length, 36);
  for (const shape of rustCursorShapes) {
    assert.deepEqual(
      parseControlMessage({ type: "cursor", visible: true, shape }),
      { type: "cursor", visible: true, shape, position: null },
    );
  }
});

test("cursor parser accepts known shapes and rejects unknown ones", () => {
  assert.deepEqual(
    parseControlMessage({ type: "cursor", visible: true, shape: "pointer" }),
    { type: "cursor", visible: true, shape: "pointer", position: null },
  );
  assert.equal(
    parseControlMessage({ type: "cursor", visible: true, shape: "magic" }),
    null,
  );
});

test("cursor parser requires normalized integer coordinates", () => {
  for (const position of [
    { x: -1, y: 0 },
    { x: 0, y: 65_536 },
    { x: 1.5, y: 2 },
  ]) {
    assert.equal(
      parseControlMessage({
        type: "cursor",
        visible: true,
        shape: "default",
        position,
      }),
      null,
    );
  }
  assert.deepEqual(
    parseControlMessage({
      type: "cursor",
      visible: true,
      shape: "default",
      position: { x: 0, y: 65_535 },
    }),
    {
      type: "cursor",
      visible: true,
      shape: "default",
      position: { x: 0, y: 65_535 },
    },
  );
});

test("observer, absolute control, pointer lock, and hidden states choose one remote cursor", () => {
  const { cursor, element, overlay, state } = fixture();
  cursor.setVideoDimensions(1600, 900);
  cursor.update(state);
  assert.equal(element.style.cursor, "crosshair");
  assert.equal(overlay.style.display, "block");

  cursor.setActive(true);
  assert.equal(element.style.cursor, "pointer");
  assert.equal(overlay.style.display, "none");
  assert.deepEqual(cursor.position(), { x: 10, y: 20 });

  cursor.setPointerLocked(true);
  assert.equal(element.style.cursor, "none");
  assert.equal(overlay.style.display, "block");

  cursor.update({ ...state, visible: false });
  assert.equal(element.style.cursor, "none");
  assert.equal(overlay.style.display, "none");
  assert.deepEqual(cursor.position(), { x: 10, y: 20 });

  cursor.update(state);
  cursor.setPointerLocked(false);
  assert.equal(element.style.cursor, "pointer");
  assert.equal(overlay.style.display, "none");

  cursor.setActive(false);
  assert.equal(element.style.cursor, "crosshair");
  assert.equal(element.style.priority, "important");
  assert.equal(overlay.style.display, "block");
});

test("overlay maps asymmetric normalized coordinates through letterbox and pillarbox bounds", () => {
  const { cursor, display, overlay, state } = fixture();
  const position = { x: 16_384, y: 49_151 };
  const located = { ...state, position };
  const xFraction = position.x / 65_535;
  const yFraction = position.y / 65_535;

  cursor.setVideoDimensions(1600, 900);
  cursor.update(located);
  const letterbox = overlayPosition(overlay);
  const letterboxHeight = 1000 / (16 / 9);
  assert.ok(Math.abs(letterbox.x - (10 + xFraction * 1000)) < 0.001);
  assert.ok(
    Math.abs(
      letterbox.y -
        (20 + (800 - letterboxHeight) / 2 + yFraction * letterboxHeight),
    ) < 0.001,
  );

  display.rect = { width: 1200, height: 500, left: 30, top: 40 };
  cursor.setVideoDimensions(800, 450);
  const pillarbox = overlayPosition(overlay);
  const pillarboxWidth = 500 * (16 / 9);
  assert.ok(
    Math.abs(
      pillarbox.x -
        (30 + (1200 - pillarboxWidth) / 2 + xFraction * pillarboxWidth),
    ) < 0.001,
  );
  assert.ok(Math.abs(pillarbox.y - (40 + yFraction * 500)) < 0.001);
});

test("unchanged frame dimensions skip layout work and captured scroll refreshes it", () => {
  const { cursor, display, overlay, state, window } = fixture();
  let layoutReads = 0;
  display.getBoundingClientRect = () => {
    layoutReads += 1;
    return { ...display.rect };
  };
  cursor.setVideoDimensions(1600, 900);
  cursor.update(state);
  assert.equal(layoutReads, 1);

  cursor.setVideoDimensions(1600, 900);
  assert.equal(layoutReads, 1);

  display.rect.left = 110;
  window.dispatch("scroll", {});
  assert.equal(layoutReads, 2);
  assert.ok(overlayPosition(overlay).x > 110);
});

test("fullscreen changes keep the overlay inside the target containing the display", () => {
  const { cursor, body, document, host, overlay, state } = fixture();
  cursor.setVideoDimensions(1600, 900);
  cursor.update(state);
  assert.equal(overlay.parent, body);

  Object.assign(document, { fullscreenElement: host });
  document.dispatch("fullscreenchange", {});
  assert.equal(overlay.parent, host);
  assert.equal(host.contains(overlay), true);

  Object.assign(document, { fullscreenElement: null });
  document.dispatch("fullscreenchange", {});
  assert.equal(overlay.parent, body);
});

test("clear and dispose remove stale overlay state and restore the native cursor", () => {
  const { body, cursor, element, host, overlay, state } = fixture();
  cursor.setVideoDimensions(1600, 900);
  cursor.update(state);
  assert.equal(overlay.style.display, "block");

  cursor.clear();
  assert.equal(overlay.style.display, "none");
  assert.deepEqual(cursor.position(), null);

  cursor.update(state);
  assert.equal(
    overlay.style.display,
    "block",
    "a cursor event after release reuses the presented frame dimensions",
  );

  cursor.setActive(true);
  assert.equal(element.style.cursor, "pointer");

  cursor.dispose();
  cursor.update({ ...state, shape: "wait", position: null });
  cursor.setActive(true);

  assert.equal(element.style.cursor, "crosshair");
  assert.equal(element.style.priority, "important");
  assert.deepEqual(cursor.position(), null);
  assert.deepEqual(body.children, [host]);
});
