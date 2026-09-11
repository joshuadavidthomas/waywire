import assert from "node:assert/strict";
import test from "node:test";

import { LocalCursor } from "./cursor.ts";
import { parseControlMessage } from "./messages.ts";
import { FakeTarget } from "./test-support.ts";

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
  const element = new FakeTarget();
  element.style.cursor = "crosshair";
  element.style.priority = "important";
  const cursor = new LocalCursor(element as unknown as HTMLElement);
  const state = {
    type: "cursor" as const,
    visible: true,
    shape: "pointer" as const,
    position: { x: 10, y: 20 },
  };
  return { cursor, element, state };
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

test("active local cursor follows shape and visibility", () => {
  const { cursor, element, state } = fixture();
  cursor.update(state);
  cursor.setActive(true);
  assert.equal(element.style.cursor, "pointer");
  assert.deepEqual(cursor.position(), { x: 10, y: 20 });

  cursor.update({ ...state, visible: false });
  assert.equal(element.style.cursor, "none");
  assert.deepEqual(cursor.position(), { x: 10, y: 20 });

  cursor.setActive(false);
  assert.equal(element.style.cursor, "crosshair");
  assert.equal(element.style.priority, "important");
});

test("dispose restores the original cursor and ignores later updates", () => {
  const { cursor, element, state } = fixture();
  cursor.setActive(true);
  cursor.update(state);
  assert.equal(element.style.cursor, "pointer");

  cursor.dispose();
  cursor.update({ ...state, shape: "wait", position: null });
  cursor.setActive(true);

  assert.equal(element.style.cursor, "crosshair");
  assert.equal(element.style.priority, "important");
  assert.deepEqual(cursor.position(), null);
});
