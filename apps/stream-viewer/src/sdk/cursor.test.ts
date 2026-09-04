import assert from "node:assert/strict";
import test from "node:test";
import { __testing } from "./waymote.ts";

type DeferredImage = {
  readonly resolve: () => void;
  readonly reject: (cause: unknown) => void;
  readonly bitmap: { naturalWidth: number; naturalHeight: number };
};

type CursorStyle = {
  cursor: string;
  priority: string;
  getPropertyValue(): string;
  getPropertyPriority(): string;
  setProperty(name: string, value: string, priority?: string): void;
  removeProperty(name: string): void;
};

function fixture() {
  const images: DeferredImage[] = [];
  const draws: unknown[][] = [];
  const errors: Error[] = [];
  class FakeImage {
    naturalWidth = 32;
    naturalHeight = 32;
    decode(): Promise<void> {
      return new Promise((resolve, reject) =>
        images.push({ resolve, reject, bitmap: this }),
      );
    }
  }
  Object.defineProperty(globalThis, "Image", {
    configurable: true,
    value: FakeImage,
  });
  Object.defineProperty(globalThis, "document", {
    configurable: true,
    value: {
      createElement(name: string) {
        assert.equal(name, "canvas");
        return {
          width: 0,
          height: 0,
          getContext: () => ({
            drawImage: (...args: unknown[]) => {
              draws.push(args);
            },
          }),
          toDataURL: () => "data:image/png;base64,AQ==",
        };
      },
    },
  });
  const style: CursorStyle = {
    cursor: "crosshair",
    priority: "important",
    getPropertyValue() {
      return this.cursor;
    },
    getPropertyPriority() {
      return this.priority;
    },
    setProperty(_name, value, priority = "") {
      this.cursor = value;
      this.priority = priority;
    },
    removeProperty() {
      this.cursor = "";
      this.priority = "";
    },
  };
  const scale = { x: 0.5, y: 0.5 };
  const element = { style } as unknown as HTMLElement;
  const cursor = new __testing.LocalCursor(
    element,
    () => scale,
    (error) => errors.push(error),
  );
  const state = {
    type: "cursor" as const,
    visible: true,
    width: 32,
    height: 32,
    hotspotX: 4,
    hotspotY: 6,
    image: "data:image/png;base64,AQ==",
  };
  return { cursor, style, scale, state, images, draws, errors };
}
const flush = async (): Promise<void> => {
  await Promise.resolve();
  await Promise.resolve();
};

test("cached cursor applies on acquire, scales its hotspot, hides, and restores on release", async () => {
  const f = fixture();
  f.cursor.update(f.state);
  f.images[0]?.resolve();
  await flush();
  assert.equal(
    f.style.cursor,
    "crosshair",
    "cursor message can precede the active lease",
  );
  f.cursor.setActive(true);
  assert.match(f.style.cursor, / 2 3, default$/);
  assert.deepEqual(f.draws[0]?.slice(1), [0, 0, 16, 16]);
  f.cursor.refresh();
  assert.equal(f.draws.length, 1, "unchanged frames do not re-encode PNGs");
  f.cursor.update({ ...f.state, visible: false });
  assert.equal(f.style.cursor, "none");
  f.cursor.update(f.state);
  assert.match(f.style.cursor, / 2 3, default$/);
  f.scale.x = f.scale.y = 8;
  f.cursor.refresh();
  assert.deepEqual(f.draws.at(-1)?.slice(1), [0, 0, 128, 128]);
  assert.match(f.style.cursor, / 16 24, default$/);
  f.cursor.setActive(false);
  assert.equal(f.style.cursor, "crosshair");
  assert.equal(f.style.priority, "important");
  f.cursor.dispose();
  assert.deepEqual(f.errors, []);
});

test("late image decoding cannot replace a newer cursor or a disposed surface", async () => {
  const f = fixture();
  f.cursor.setActive(true);
  f.cursor.update(f.state);
  f.cursor.update({
    ...f.state,
    image: "data:image/png;base64,Ag==",
    hotspotX: 10,
  });
  f.images[1]?.resolve();
  await flush();
  assert.match(f.style.cursor, / 5 3, default$/);
  f.images[0]?.resolve();
  await flush();
  assert.match(f.style.cursor, / 5 3, default$/);
  f.cursor.update({ ...f.state, image: "data:image/png;base64,Aw==" });
  f.cursor.dispose();
  f.images[2]?.resolve();
  await flush();
  assert.equal(f.style.cursor, "crosshair");
  assert.equal(f.style.priority, "important");
});

test("reset and malformed images restore the local cursor without stale async work", async () => {
  const f = fixture();
  f.cursor.setActive(true);
  assert.throws(
    () => f.cursor.update({ ...f.state, width: 257 }),
    /Invalid remote cursor/,
  );
  assert.throws(
    () => f.cursor.update({ ...f.state, image: "https://example.com/cursor" }),
    /Invalid remote cursor/,
  );
  f.cursor.update(f.state);
  const first = f.images[0];
  if (!first) throw new Error("image decode was not requested");
  first.bitmap.naturalWidth = 64;
  first.resolve();
  await flush();
  assert.equal(f.errors.length, 1);
  assert.equal(f.style.cursor, "crosshair");
  f.cursor.update(f.state);
  f.cursor.update({ ...f.state, image: "", width: 0, height: 0 });
  f.images[1]?.resolve();
  await flush();
  assert.equal(f.style.cursor, "crosshair");
  f.cursor.dispose();
});
