import {
  keyboardKey,
  pointerAbsolute,
  pointerButton,
  pointerRelative,
  pointerScroll,
  releaseAll,
  type Button,
} from "./wire.ts";

export class SurfaceListeners {
  private readonly cleanup: Array<() => void> = [];

  add<E extends Event>(
    target: EventTarget,
    type: string,
    listener: (event: E) => void,
    options?: AddEventListenerOptions | boolean,
  ): void {
    const adapted: EventListener = (event) => listener(event as E);
    target.addEventListener(type, adapted, options);
    this.cleanup.push(() => target.removeEventListener(type, adapted, options));
  }

  dispose(): void {
    for (const cleanup of this.cleanup.splice(0)) cleanup();
  }
}

type PointerPosition = Readonly<{ x: number; y: number }>;

const maximumPointerDelta = 4096;
const keyReleased = 0;
const keyPressed = 1;
const keyRepeated = 2;
const linuxPointerButtons: readonly Button[] = [
  0x110, 0x112, 0x111, 0x113, 0x114,
];
const linuxKeyCodes = new Map([
  ["Escape", 1],
  ["Digit1", 2],
  ["Digit2", 3],
  ["Digit3", 4],
  ["Digit4", 5],
  ["Digit5", 6],
  ["Digit6", 7],
  ["Digit7", 8],
  ["Digit8", 9],
  ["Digit9", 10],
  ["Digit0", 11],
  ["Minus", 12],
  ["Equal", 13],
  ["Backspace", 14],
  ["Tab", 15],
  ["KeyQ", 16],
  ["KeyW", 17],
  ["KeyE", 18],
  ["KeyR", 19],
  ["KeyT", 20],
  ["KeyY", 21],
  ["KeyU", 22],
  ["KeyI", 23],
  ["KeyO", 24],
  ["KeyP", 25],
  ["BracketLeft", 26],
  ["BracketRight", 27],
  ["Enter", 28],
  ["ControlLeft", 29],
  ["KeyA", 30],
  ["KeyS", 31],
  ["KeyD", 32],
  ["KeyF", 33],
  ["KeyG", 34],
  ["KeyH", 35],
  ["KeyJ", 36],
  ["KeyK", 37],
  ["KeyL", 38],
  ["Semicolon", 39],
  ["Quote", 40],
  ["Backquote", 41],
  ["ShiftLeft", 42],
  ["Backslash", 43],
  ["KeyZ", 44],
  ["KeyX", 45],
  ["KeyC", 46],
  ["KeyV", 47],
  ["KeyB", 48],
  ["KeyN", 49],
  ["KeyM", 50],
  ["Comma", 51],
  ["Period", 52],
  ["Slash", 53],
  ["ShiftRight", 54],
  ["NumpadMultiply", 55],
  ["AltLeft", 56],
  ["Space", 57],
  ["CapsLock", 58],
  ["F1", 59],
  ["F2", 60],
  ["F3", 61],
  ["F4", 62],
  ["F5", 63],
  ["F6", 64],
  ["F7", 65],
  ["F8", 66],
  ["F9", 67],
  ["F10", 68],
  ["NumLock", 69],
  ["ScrollLock", 70],
  ["Numpad7", 71],
  ["Numpad8", 72],
  ["Numpad9", 73],
  ["NumpadSubtract", 74],
  ["Numpad4", 75],
  ["Numpad5", 76],
  ["Numpad6", 77],
  ["NumpadAdd", 78],
  ["Numpad1", 79],
  ["Numpad2", 80],
  ["Numpad3", 81],
  ["Numpad0", 82],
  ["NumpadDecimal", 83],
  ["IntlBackslash", 86],
  ["F11", 87],
  ["F12", 88],
  ["NumpadEnter", 96],
  ["ControlRight", 97],
  ["NumpadDivide", 98],
  ["PrintScreen", 99],
  ["AltRight", 100],
  ["Home", 102],
  ["ArrowUp", 103],
  ["PageUp", 104],
  ["ArrowLeft", 105],
  ["ArrowRight", 106],
  ["End", 107],
  ["ArrowDown", 108],
  ["PageDown", 109],
  ["Insert", 110],
  ["Delete", 111],
  ["AudioVolumeMute", 113],
  ["AudioVolumeDown", 114],
  ["AudioVolumeUp", 115],
  ["Power", 116],
  ["NumpadEqual", 117],
  ["Pause", 119],
  ["MetaLeft", 125],
  ["MetaRight", 126],
  ["ContextMenu", 127],
  ["BrowserStop", 128],
  ["Again", 129],
  ["Props", 130],
  ["Undo", 131],
  ["Copy", 133],
  ["Open", 134],
  ["Paste", 135],
  ["Find", 136],
  ["Cut", 137],
  ["Help", 138],
  ["Menu", 139],
  ["Sleep", 142],
  ["WakeUp", 143],
  ["BrowserFavorites", 156],
  ["BrowserBack", 158],
  ["BrowserForward", 159],
  ["Eject", 161],
  ["MediaTrackNext", 163],
  ["MediaPlayPause", 164],
  ["MediaTrackPrevious", 165],
  ["MediaStop", 166],
  ["BrowserHome", 172],
  ["BrowserRefresh", 173],
  ["F13", 183],
  ["F14", 184],
  ["F15", 185],
  ["F16", 186],
  ["F17", 187],
  ["F18", 188],
  ["F19", 189],
  ["F20", 190],
  ["F21", 191],
  ["F22", 192],
  ["F23", 193],
  ["F24", 194],
]);

export interface InputOwner {
  display(): HTMLCanvasElement | null;
  inputElement(): HTMLElement | null;
  imeProxy(): HTMLInputElement | HTMLTextAreaElement | null;
  contentPosition(event: PointerEvent): PointerPosition | null;
  sendRecord(record: ArrayBuffer): boolean;
  nextSequence(): number;
  requestControl(): void;
  releaseControl(): void;
  controlActive(): boolean;
  controlOnFocus(): boolean;
  sendLocalClipboard(): Promise<boolean>;
  disposed(): boolean;
  setClipboardUnavailable(error: unknown): void;
  armRemoteCopy(): void;
  completeRemoteCopy(): void;
  sendText(action: "preedit" | "commit", text: string): void;
  updatePointerLocked(locked: boolean): void;
  visibilityChanged(): void;
  emitError(error: Error): void;
}

export class InputRuntime {
  private readonly listeners: Array<() => void> = [];
  private readonly pressedKeys = new Set<number>();
  private readonly pressedButtons = new Set<number>();
  private pendingPointerPosition: PointerPosition | null = null;
  private pointerAnimationPending = false;
  private pointerAnimationFrame: number | null = null;
  private physicalTextPending = false;
  private suppressCompositionText: string | null = null;
  private compositionTimer: ReturnType<typeof setTimeout> | null = null;
  private clipboardPastePending = false;

  constructor(private readonly owner: InputOwner) {}

  attach(
    input: HTMLElement,
    ime: HTMLInputElement | HTMLTextAreaElement,
  ): void {
    this.add(input, "pointermove", this.queuePointerPosition);
    this.add(input, "pointermove", this.handleRelativePointerMove);
    this.add(input, "pointerdown", this.handlePointerDown);
    this.add(input, "pointerup", this.handlePointerUp);
    this.add(input, "pointercancel", this.release);
    this.add(input, "contextmenu", (event) => event.preventDefault());
    this.add(input, "wheel", this.handleWheel, { passive: false });
    this.add(input, "keydown", this.handleKeyDown);
    this.add(input, "keyup", this.handleKeyUp);
    this.add(input, "focus", () => {
      if (this.owner.controlOnFocus()) this.owner.requestControl();
    });
    this.add(input, "blur", (event: FocusEvent) => {
      if (event.relatedTarget !== this.owner.imeProxy())
        this.owner.releaseControl();
    });
    if (ime !== input) {
      this.add(ime, "keydown", this.handleKeyDown);
      this.add(ime, "keyup", this.handleKeyUp);
    }
    this.add(ime, "compositionupdate", this.handleCompositionUpdate);
    this.add(ime, "compositionend", this.handleCompositionEnd);
    this.add(ime, "beforeinput", this.handleBeforeInput);
    this.add(ime, "focus", () => this.owner.requestControl());
    this.add(ime, "blur", this.handleTextInputBlur);
    this.add(document, "pointerlockchange", this.handlePointerLockChange);
    this.add(document, "pointerlockerror", () =>
      this.owner.emitError(new Error("The browser denied pointer lock")),
    );
    this.add(document, "visibilitychange", () =>
      this.owner.visibilityChanged(),
    );
  }

  dispose(): void {
    this.resetPressed();
    this.cancelAnimation();
    for (const cleanup of this.listeners.splice(0)) cleanup();
    if (this.compositionTimer !== null) clearTimeout(this.compositionTimer);
    this.compositionTimer = null;
    this.suppressCompositionText = null;
    this.clipboardPastePending = false;
  }

  resetPressed(): void {
    this.pendingPointerPosition = null;
    this.pressedKeys.clear();
    this.pressedButtons.clear();
  }

  release = (): void => {
    this.resetPressed();
    this.owner.sendRecord(releaseAll());
  };

  cancelAnimation(): void {
    if (this.pointerAnimationFrame !== null)
      cancelAnimationFrame(this.pointerAnimationFrame);
    this.pointerAnimationFrame = null;
    this.pointerAnimationPending = false;
    this.pendingPointerPosition = null;
  }

  private add<E extends Event>(
    target: EventTarget,
    type: string,
    listener: (event: E) => void,
    options?: AddEventListenerOptions | boolean,
  ): void {
    const adapted: EventListener = (event) => listener(event as E);
    target.addEventListener(type, adapted, options);
    this.listeners.push(() =>
      target.removeEventListener(type, adapted, options),
    );
  }

  private queuePointerPosition = (event: PointerEvent): void => {
    if (document.pointerLockElement === this.owner.display()) return;
    this.pendingPointerPosition = this.owner.contentPosition(event);
    if (!this.pendingPointerPosition || this.pointerAnimationPending) return;
    this.pointerAnimationPending = true;
    this.pointerAnimationFrame = requestAnimationFrame(() => {
      this.pointerAnimationFrame = null;
      this.pointerAnimationPending = false;
      const position = this.pendingPointerPosition;
      this.pendingPointerPosition = null;
      if (position)
        this.owner.sendRecord(
          pointerAbsolute(position.x, position.y, this.owner.nextSequence()),
        );
    });
  };

  private handleRelativePointerMove = (event: PointerEvent): void => {
    if (document.pointerLockElement !== this.owner.display()) return;
    const dx = Math.max(
      -maximumPointerDelta,
      Math.min(maximumPointerDelta, event.movementX),
    );
    const dy = Math.max(
      -maximumPointerDelta,
      Math.min(maximumPointerDelta, event.movementY),
    );
    this.owner.sendRecord(pointerRelative(dx, dy, this.owner.nextSequence()));
  };

  private handlePointerLockChange = (): void => {
    const locked = document.pointerLockElement === this.owner.display();
    this.owner.updatePointerLocked(locked);
    if (!locked) this.release();
  };

  private handlePointerDown = (event: PointerEvent): void => {
    const display = this.owner.display();
    const locked = document.pointerLockElement === display;
    const position = locked ? null : this.owner.contentPosition(event);
    if ((!locked && !position) || event.button < 0 || event.button > 4) return;
    const input = this.owner.inputElement();
    if (!input) return;
    input.focus({ preventScroll: true });
    if (!locked) input.setPointerCapture(event.pointerId);
    if (position)
      this.owner.sendRecord(
        pointerAbsolute(position.x, position.y, this.owner.nextSequence()),
      );
    const button = linuxPointerButtons[event.button];
    if (button !== undefined && !this.pressedButtons.has(event.button)) {
      this.pressedButtons.add(event.button);
      this.owner.sendRecord(
        pointerButton(button, keyPressed, this.owner.nextSequence()),
      );
    }
    event.preventDefault();
  };

  private handlePointerUp = (event: PointerEvent): void => {
    this.queuePointerPosition(event);
    const button = linuxPointerButtons[event.button];
    if (button !== undefined && this.pressedButtons.delete(event.button)) {
      this.owner.sendRecord(
        pointerButton(button, keyReleased, this.owner.nextSequence()),
      );
    }
    const input = this.owner.inputElement();
    if (input?.hasPointerCapture(event.pointerId))
      input.releasePointerCapture(event.pointerId);
    event.preventDefault();
  };

  private handleWheel = (event: WheelEvent): void => {
    this.owner.inputElement()?.focus({ preventScroll: true });
    const scale =
      event.deltaMode === WheelEvent.DOM_DELTA_LINE
        ? 16
        : event.deltaMode === WheelEvent.DOM_DELTA_PAGE
          ? (this.owner.display()?.clientHeight ?? 1)
          : 1;
    const dx = Math.max(
      -maximumPointerDelta,
      Math.min(maximumPointerDelta, event.deltaX * scale),
    );
    const dy = Math.max(
      -maximumPointerDelta,
      Math.min(maximumPointerDelta, event.deltaY * scale),
    );
    this.owner.sendRecord(pointerScroll(dx, dy, this.owner.nextSequence()));
    event.preventDefault();
  };

  private modifiers(event: KeyboardEvent): number[] {
    const modifiers = [...this.pressedKeys].filter((key) =>
      [29, 42, 54, 97, 100, 125, 126].includes(key),
    );
    if (event.ctrlKey && !modifiers.some((key) => key === 29 || key === 97))
      modifiers.push(29);
    if (event.shiftKey && !modifiers.some((key) => key === 42 || key === 54))
      modifiers.push(42);
    if (event.altKey && !modifiers.includes(56) && !modifiers.includes(100))
      modifiers.push(56);
    if (event.metaKey && !modifiers.includes(125) && !modifiers.includes(126))
      modifiers.push(125);
    return modifiers;
  }

  private tap(key: number, modifiers: readonly number[]): void {
    for (const modifier of modifiers)
      this.owner.sendRecord(
        keyboardKey(modifier, keyPressed, this.owner.nextSequence()),
      );
    this.owner.sendRecord(
      keyboardKey(key, keyPressed, this.owner.nextSequence()),
    );
    this.owner.sendRecord(
      keyboardKey(key, keyReleased, this.owner.nextSequence()),
    );
    for (const modifier of modifiers)
      if (!this.pressedKeys.has(modifier))
        this.owner.sendRecord(
          keyboardKey(modifier, keyReleased, this.owner.nextSequence()),
        );
  }

  private handleKeyDown = (event: KeyboardEvent): void => {
    if (event.isComposing || event.keyCode === 229) return;
    const key = linuxKeyCodes.get(event.code);
    if (key === undefined) return;
    if ((event.ctrlKey || event.metaKey) && event.code === "KeyV") {
      if (!event.repeat && !this.clipboardPastePending) {
        this.clipboardPastePending = true;
        const modifiers = this.modifiers(event);
        this.owner
          .sendLocalClipboard()
          .catch((error) => {
            if (!this.owner.disposed())
              this.owner.setClipboardUnavailable(error);
          })
          .finally(() => {
            if (!this.owner.disposed()) this.tap(key, modifiers);
            this.clipboardPastePending = false;
          });
      }
      event.preventDefault();
      event.stopPropagation();
      return;
    }
    if (
      (event.ctrlKey || event.metaKey) &&
      event.shiftKey &&
      event.code === "KeyC"
    ) {
      if (!event.repeat) {
        this.owner.armRemoteCopy();
        this.tap(key, this.modifiers(event));
      }
      event.preventDefault();
      event.stopPropagation();
      return;
    }
    if (!this.pressedKeys.has(key)) {
      this.pressedKeys.add(key);
      this.owner.sendRecord(
        keyboardKey(key, keyPressed, this.owner.nextSequence()),
      );
    } else if (event.repeat)
      this.owner.sendRecord(
        keyboardKey(key, keyRepeated, this.owner.nextSequence()),
      );
    this.physicalTextPending =
      event.key?.length === 1 && !event.ctrlKey && !event.metaKey;
    event.preventDefault();
    event.stopPropagation();
  };

  private handleKeyUp = (event: KeyboardEvent): void => {
    if (event.isComposing || event.keyCode === 229) return;
    const key = linuxKeyCodes.get(event.code);
    if (key === undefined) return;
    if (
      [
        "KeyC",
        "ShiftLeft",
        "ShiftRight",
        "ControlLeft",
        "ControlRight",
        "MetaLeft",
        "MetaRight",
      ].includes(event.code)
    )
      this.owner.completeRemoteCopy();
    if (this.pressedKeys.delete(key))
      this.owner.sendRecord(
        keyboardKey(key, keyReleased, this.owner.nextSequence()),
      );
    this.physicalTextPending = false;
    event.preventDefault();
    event.stopPropagation();
  };

  private handleCompositionUpdate = (event: CompositionEvent): void => {
    if (this.owner.controlActive()) this.owner.sendText("preedit", event.data);
  };

  private handleCompositionEnd = (event: CompositionEvent): void => {
    if (this.owner.controlActive()) this.owner.sendText("commit", event.data);
    this.suppressCompositionText = event.data;
    if (this.compositionTimer !== null) clearTimeout(this.compositionTimer);
    this.compositionTimer = setTimeout(() => {
      this.compositionTimer = null;
      if (this.suppressCompositionText === event.data)
        this.suppressCompositionText = null;
    }, 0);
    const ime = this.owner.imeProxy();
    if (ime) ime.value = "";
  };

  private handleBeforeInput = (event: InputEvent): void => {
    if (event.isComposing || event.inputType !== "insertText" || !event.data)
      return;
    if (
      this.physicalTextPending ||
      this.suppressCompositionText === event.data
    ) {
      this.physicalTextPending = false;
      this.suppressCompositionText = null;
      event.preventDefault();
    } else if (this.owner.controlActive()) {
      this.owner.sendText("commit", event.data);
      event.preventDefault();
    }
    const ime = this.owner.imeProxy();
    if (ime) ime.value = "";
  };

  private handleTextInputBlur = (event: FocusEvent): void => {
    if (this.owner.controlActive()) this.owner.sendText("preedit", "");
    const ime = this.owner.imeProxy();
    if (ime) ime.value = "";
    if (event.relatedTarget !== this.owner.display())
      this.owner.releaseControl();
  };
}
