import { ProtocolVersionMismatchError } from "../sdk/messages.ts";
import { WaywireSession, type SurfaceHandle } from "../sdk/waywire.ts";
import {
  installViewerListeners,
  LATENCY_STORAGE_KEY,
  type ViewerControls,
  type ViewerElements,
} from "./viewer-listeners.ts";

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
  const input = (element: Element): element is HTMLInputElement =>
    element instanceof HTMLInputElement;
  return {
    stage: requireElement("#stage", html),
    display: requireElement(
      "#display",
      (element): element is HTMLCanvasElement =>
        element instanceof HTMLCanvasElement,
    ),
    signal: requireElement("#signal", html),
    signalHeadline: requireElement("#signal-headline", html),
    signalMessage: requireElement("#signal-message", html),
    leaseNotice: requireElement("#lease-notice", html),
    hud: requireElement("#hud", html),
    menuKey: requireElement("#menu-key", button),
    panel: requireElement("#panel", html),
    status: requireElement("#status", html),
    statusVideo: requireElement("#status-video", html),
    controlToggle: requireElement("#control-toggle", input),
    pointerLockButton: requireElement("#pointer-lock", button),
    keyboardButton: requireElement("#keyboard", button),
    sendClipboardButton: requireElement("#send-clipboard", button),
    copyClipboardButton: requireElement("#copy-clipboard", button),
    hudToggle: requireElement("#hud-toggle", input),
    resetVideoButton: requireElement("#reset-video", button),
    clipboardStatus: requireElement("#clipboard-status", html),
    latency: requireElement("#latency", html),
    fullscreenButton: requireElement("#fullscreen", button),
    pinButton: requireElement("#pin-panel", button),
    closeButton: requireElement("#close-panel", button),
    imeProxy: requireElement("#ime-proxy", input),
  };
}

// The latency choice sticks between visits when storage allows it.
function checkedLatency(latency: HTMLElement): number {
  let stored: string | null = null;
  try {
    stored = localStorage.getItem(LATENCY_STORAGE_KEY);
  } catch {
    stored = null;
  }
  for (const input of latency.querySelectorAll("input")) {
    if (input.value === stored) input.checked = true;
  }
  const checked = latency.querySelector("input:checked");
  return checked instanceof HTMLInputElement ? Number(checked.value) : 60;
}

function startViewer(): void {
  const elements = viewerElements();
  const session = new WaywireSession({
    latency: checkedLatency(elements.latency),
    statsIntervalMs: 250,
    remoteDisplay: {
      mode: "observe",
      element: elements.display,
      devicePixelRatio: 1,
    },
  });
  const listeners = new AbortController();
  let surface: SurfaceHandle | undefined;
  let viewer: ViewerControls | undefined;
  let removeProtocolVersionListener: (() => void) | undefined;
  let disposed = false;
  function dispose(): void {
    if (disposed) return;
    disposed = true;
    listeners.abort();
    viewer?.dispose();
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
        viewer?.settle();
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
    viewer = installViewerListeners(elements, session, attachedSurface);
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
        elements.clipboardStatus.textContent =
          "The browser would not hand over your clipboard";
      }
    });
    onPermissionClick(elements.copyClipboardButton, async () => {
      const text = session.clipboard.latestRemoteText;
      if (text === null) return;
      try {
        await navigator.clipboard.writeText(text);
        if (!disposed) elements.clipboardStatus.textContent = "";
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
          ? ""
          : "The browser would not take the remote clipboard";
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
