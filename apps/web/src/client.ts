import { Effect } from "effect";

import { acquireWaymoteSession } from "./sdk/effect.ts";
import {
  installViewerListeners,
  type ViewerElements,
} from "./sdk/viewer-listeners.ts";

function requireElement<T extends Element>(
  selector: string,
  isExpectedElement: (element: Element) => element is T,
): T {
  const element = document.querySelector(selector);
  if (!element || !isExpectedElement(element)) {
    throw new Error(`Expected ${selector} in the viewer document`);
  }
  return element;
}

function viewerElements(): ViewerElements {
  const html = (element: Element): element is HTMLElement =>
    element instanceof HTMLElement;
  const button = (element: Element): element is HTMLButtonElement =>
    element instanceof HTMLButtonElement;
  return {
    display: requireElement(
      "#display",
      (element): element is HTMLCanvasElement =>
        element instanceof HTMLCanvasElement,
    ),
    empty: requireElement("#empty", html),
    status: requireElement("#status", html),
    controlStatus: requireElement("#control-status", html),
    codec: requireElement("#codec", html),
    metrics: requireElement("#metrics", html),
    latency: requireElement(
      "#latency",
      (element): element is HTMLSelectElement =>
        element instanceof HTMLSelectElement,
    ),
    pointerLockButton: requireElement("#pointer-lock", button),
    textInputButton: requireElement("#text-input", button),
    sendClipboardButton: requireElement("#send-clipboard", button),
    copyClipboardButton: requireElement("#copy-clipboard", button),
    clipboardStatus: requireElement("#clipboard-status", html),
    imeProxy: requireElement(
      "#ime-proxy",
      (element): element is HTMLInputElement =>
        element instanceof HTMLInputElement,
    ),
  };
}

function waitForEvent(target: EventTarget, type: string): Effect.Effect<Event> {
  return Effect.async<Event>((resume) => {
    const listener = (event: Event): void => resume(Effect.succeed(event));
    target.addEventListener(type, listener, { once: true });
    return Effect.sync(() => target.removeEventListener(type, listener));
  });
}

function permissionTasks(
  elements: ViewerElements,
  session: import("./sdk/waymote.ts").WaymoteSession,
  surface: import("./sdk/waymote.ts").SurfaceHandle,
): Effect.Effect<never> {
  const pointerLock = Effect.forever(
    Effect.flatMap(waitForEvent(elements.pointerLockButton, "click"), () =>
      document.pointerLockElement === elements.display
        ? Effect.sync(() => surface.exitPointerLock())
        : Effect.tryPromise({
            try: () => surface.requestPointerLock(),
            catch: (cause) =>
              cause instanceof Error ? cause : new Error(String(cause)),
          }).pipe(
            Effect.catchAll((error) =>
              Effect.sync(() => console.warn("pointer lock failed", error)),
            ),
          ),
    ),
  );
  const sendClipboard = Effect.forever(
    Effect.flatMap(waitForEvent(elements.sendClipboardButton, "click"), () =>
      Effect.tryPromise({
        try: () => navigator.clipboard.readText(),
        catch: (cause) =>
          cause instanceof Error ? cause : new Error(String(cause)),
      }).pipe(
        Effect.tap((text) =>
          Effect.sync(() => {
            session.clipboard.sendText(text);
          }),
        ),
        Effect.catchAll((error) =>
          Effect.sync(() => {
            console.warn("local clipboard read failed", error);
            elements.clipboardStatus.textContent =
              "Local clipboard unavailable";
          }),
        ),
      ),
    ),
  );
  const copyClipboard = Effect.forever(
    Effect.flatMap(waitForEvent(elements.copyClipboardButton, "click"), () => {
      const text = session.clipboard.latestRemoteText;
      if (text === null) return Effect.void;
      return Effect.tryPromise({
        try: () => navigator.clipboard.writeText(text),
        catch: (cause) =>
          cause instanceof Error ? cause : new Error(String(cause)),
      }).pipe(
        Effect.tap(() =>
          Effect.sync(() => {
            elements.clipboardStatus.textContent = "Remote clipboard copied";
          }),
        ),
        Effect.catchAll((error) =>
          Effect.sync(() => {
            let copied = false;
            const handleCopy = (event: ClipboardEvent): void => {
              event.clipboardData?.setData("text/plain", text);
              event.preventDefault();
              copied = true;
            };
            document.addEventListener("copy", handleCopy);
            document.execCommand("copy");
            document.removeEventListener("copy", handleCopy);
            elements.clipboardStatus.textContent = copied
              ? "Remote clipboard copied"
              : "Clipboard write unavailable";
            if (!copied) console.warn("remote clipboard write failed", error);
          }),
        ),
      );
    }),
  );
  return Effect.andThen(
    Effect.all([pointerLock, sendClipboard, copyClipboard], {
      concurrency: "unbounded",
    }),
    Effect.never,
  );
}

export const viewerProgram: Effect.Effect<void, Error> = Effect.scoped(
  Effect.gen(function* () {
    const elements = viewerElements();
    const { session, surface } = yield* acquireWaymoteSession(
      {
        latency: Number(elements.latency.value),
        statsIntervalMs: 250,
        remoteDisplay: {
          mode: "observe",
          element: elements.display,
          devicePixelRatio: 1,
        },
      },
      {
        canvas: elements.display,
        inputElement: elements.display,
        textInputElement: elements.imeProxy,
        controlOnFocus: true,
        clipboardAutoSync: true,
      },
    );
    yield* Effect.acquireRelease(
      Effect.sync(() =>
        installViewerListeners(elements, session, surface),
      ),
      (removeListeners) => Effect.sync(removeListeners),
    );
    yield* Effect.race(
      waitForEvent(window, "pagehide"),
      permissionTasks(elements, session, surface),
    );
  }),
);

Effect.runPromise(viewerProgram).catch((error: unknown) => {
  console.error("Viewer stopped", error);
});
