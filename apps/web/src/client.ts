import { ProtocolVersionMismatchError } from "./sdk/messages.ts";
import { WaywireSession, type SurfaceHandle } from "./sdk/waywire.ts";
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
    resetVideoButton: requireElement("#reset-video", button),
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

function startViewer(): void {
  const elements = viewerElements();
  const session = new WaywireSession({
    latency: Number(elements.latency.value),
    statsIntervalMs: 250,
    remoteDisplay: {
      mode: "observe",
      element: elements.display,
      devicePixelRatio: 1,
    },
  });
  const listeners = new AbortController();
  let surface: SurfaceHandle | undefined;
  let removeViewerListeners: (() => void) | undefined;
  let removeProtocolVersionListener: (() => void) | undefined;
  let disposed = false;
  function dispose(): void {
    if (disposed) return;
    disposed = true;
    listeners.abort();
    removeViewerListeners?.();
    removeProtocolVersionListener?.();
    surface?.dispose();
    void session
      .dispose()
      .catch((error: unknown) => console.error("Viewer stopped", error));
  }

  // Each permission request keeps its user gesture and ignores repeated clicks
  // until it settles. Page teardown cancels listeners and late continuations.
  function onPermissionClick(
    button: HTMLButtonElement,
    action: () => Promise<void>,
  ): void {
    let pending = false;
    button.addEventListener(
      "click",
      () => {
        if (pending || disposed) return;
        pending = true;
        void action()
          .catch((error: unknown) => {
            if (!disposed) {
              dispose();
              console.error("Viewer stopped", error);
            }
          })
          .finally(() => {
            pending = false;
          });
      },
      { signal: listeners.signal },
    );
  }

  try {
    const attachedSurface = session.attachSurface({
      canvas: elements.display,
      inputElement: elements.display,
      textInputElement: elements.imeProxy,
      controlOnFocus: true,
      clipboardAutoSync: true,
    });
    surface = attachedSurface;
    removeViewerListeners = installViewerListeners(
      elements,
      session,
      attachedSurface,
    );
    removeProtocolVersionListener = session.on("error", (error) => {
      if (!(error instanceof ProtocolVersionMismatchError)) return;
      const storageKey = "waywire.reloaded-for-protocol";
      try {
        if (sessionStorage.getItem(storageKey) === String(error.actual)) return;
        sessionStorage.setItem(storageKey, String(error.actual));
      } catch {
        // Reloading can still replace stale assets when storage is unavailable.
      }
      location.reload();
    });
    onPermissionClick(elements.pointerLockButton, async () => {
      if (document.pointerLockElement === elements.display) {
        attachedSurface.exitPointerLock();
        return;
      }
      try {
        await attachedSurface.requestPointerLock();
      } catch (error) {
        if (!disposed) console.warn("pointer lock failed", error);
      }
    });
    onPermissionClick(elements.sendClipboardButton, async () => {
      try {
        const text = await navigator.clipboard.readText();
        if (!disposed) session.clipboard.sendText(text);
      } catch (error) {
        if (disposed) return;
        console.warn("local clipboard read failed", error);
        elements.clipboardStatus.textContent = "Local clipboard unavailable";
      }
    });
    onPermissionClick(elements.copyClipboardButton, async () => {
      const text = session.clipboard.latestRemoteText;
      if (text === null) return;
      try {
        await navigator.clipboard.writeText(text);
        if (!disposed)
          elements.clipboardStatus.textContent = "Remote clipboard copied";
      } catch (error) {
        if (disposed) return;
        let copied = false;
        const handleCopy = (event: ClipboardEvent): void => {
          event.clipboardData?.setData("text/plain", text);
          event.preventDefault();
          copied = true;
        };
        document.addEventListener("copy", handleCopy);
        try {
          document.execCommand("copy");
        } finally {
          document.removeEventListener("copy", handleCopy);
        }
        elements.clipboardStatus.textContent = copied
          ? "Remote clipboard copied"
          : "Clipboard write unavailable";
        if (!copied) console.warn("remote clipboard write failed", error);
      }
    });
    window.addEventListener("pagehide", dispose, {
      once: true,
      signal: listeners.signal,
    });
    session.connect();
  } catch (error) {
    dispose();
    throw error;
  }
}

try {
  startViewer();
} catch (error) {
  console.error("Viewer stopped", error);
}
