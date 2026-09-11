import { ProtocolVersionMismatchError } from "../sdk/messages.ts";
import { WaywireSession, type SurfaceHandle } from "../sdk/waywire.ts";
import {
  installViewerListeners,
  LATENCY_STORAGE_KEY,
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
    controlStatus: requireElement("#control-status", html),
    controlToggle: requireElement("#control-toggle", button),
    pointerLockButton: requireElement("#pointer-lock", button),
    keyboardButton: requireElement("#keyboard", button),
    sendClipboardButton: requireElement("#send-clipboard", button),
    copyClipboardButton: requireElement("#copy-clipboard", button),
    hudToggle: requireElement("#hud-toggle", button),
    resetVideoButton: requireElement("#reset-video", button),
    clipboardStatus: requireElement("#clipboard-status", html),
    latency: requireElement(
      "#latency",
      (element): element is HTMLFieldSetElement =>
        element instanceof HTMLFieldSetElement,
    ),
    imeProxy: requireElement(
      "#ime-proxy",
      (element): element is HTMLInputElement =>
        element instanceof HTMLInputElement,
    ),
  };
}

// The latency choice sticks between visits when storage allows it.
function checkedLatency(latency: HTMLFieldSetElement): number {
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
  function closePanel(): void {
    if (elements.panel.matches(":popover-open")) elements.panel.hidePopover();
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
        closePanel();
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
        elements.clipboardStatus.textContent =
          "The browser refused to read the local clipboard";
      }
    });
    onPermissionClick(elements.copyClipboardButton, async () => {
      const text = session.clipboard.latestRemoteText;
      if (text === null) return;
      try {
        await navigator.clipboard.writeText(text);
        if (!disposed)
          elements.clipboardStatus.textContent = "Desktop clipboard copied";
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
          ? "Desktop clipboard copied"
          : "The browser refused to write the local clipboard";
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
