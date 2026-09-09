import assert from "node:assert/strict";
import test from "node:test";

import { LocalCursor } from "./cursor.ts";
import { FakeTarget, flush, installGlobal } from "./test-support.ts";

type DeferredImage = {
  readonly resolve: () => void;
  readonly bitmap: { naturalWidth: number; naturalHeight: number };
};

function fixture() {
  const images: DeferredImage[] = [];
  const draws: unknown[][] = [];
  class FakeImage {
    naturalWidth = 32;
    naturalHeight = 32;
    decode(): Promise<void> {
      return new Promise((resolve) => images.push({ resolve, bitmap: this }));
    }
  }
  installGlobal("Image", FakeImage);
  installGlobal("document", {
    createElement(name: string) {
      assert.equal(name, "canvas");
      return {
        width: 0,
        height: 0,
        getContext: () => ({
          drawImage: (...args: unknown[]) => draws.push(args),
        }),
        toDataURL: () => "data:image/png;base64,AQ==",
      };
    },
  });
  const element = new FakeTarget();
  element.style.cursor = "crosshair";
  element.style.priority = "important";
  const scale = { x: 0.5, y: 0.5 };
  const errors: Error[] = [];
  const cursor = new LocalCursor(
    element as unknown as HTMLElement,
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
  return { cursor, element, scale, state, images, draws, errors };
}

test("cursor preserves dimensions and hotspot while scaling", async () => {
  const f = fixture();
  f.cursor.update(f.state);
  f.images[0]?.resolve();
  await flush();
  assert.equal(f.element.style.cursor, "crosshair");
  f.cursor.setActive(true);
  assert.match(f.element.style.cursor, / 2 3, default$/);
  assert.deepEqual(f.draws[0]?.slice(1), [0, 0, 16, 16]);

  f.scale.x = f.scale.y = 8;
  f.cursor.refresh();
  assert.deepEqual(f.draws.at(-1)?.slice(1), [0, 0, 128, 128]);
  assert.match(f.element.style.cursor, / 16 24, default$/);
  f.cursor.setActive(false);
  assert.equal(f.element.style.cursor, "crosshair");
  assert.equal(f.element.style.priority, "important");
  f.cursor.dispose();
  assert.deepEqual(f.errors, []);
});

test("late cursor images cannot replace a newer version or disposed cursor", async () => {
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
  assert.match(f.element.style.cursor, / 5 3, default$/);
  f.images[0]?.resolve();
  await flush();
  assert.match(f.element.style.cursor, / 5 3, default$/);

  f.cursor.update({ ...f.state, image: "data:image/png;base64,Aw==" });
  f.cursor.dispose();
  f.images[2]?.resolve();
  await flush();
  assert.equal(f.element.style.cursor, "crosshair");
  assert.equal(f.element.style.priority, "important");
});
