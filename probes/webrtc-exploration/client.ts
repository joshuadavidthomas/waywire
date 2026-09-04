export {};

const video = document.querySelector<HTMLVideoElement>("#video")!;
const connectButton = document.querySelector<HTMLButtonElement>("#connect")!;
const stopButton = document.querySelector<HTMLButtonElement>("#stop")!;
const status = document.querySelector<HTMLElement>("#status")!;
const statsElement = document.querySelector<HTMLElement>("#stats")!;

type Numeric = number | null;
interface MediaSample {
  atMs: number;
  codec: string | null;
  fmtp: string | null;
  decoderImplementation: string | null;
  powerEfficientDecoder: boolean | null;
  framesDecoded: Numeric;
  framesDropped: Numeric;
  framesPerSecond: Numeric;
  bytesReceived: Numeric;
  totalDecodeTime: Numeric;
  totalProcessingDelay: Numeric;
  jitterBufferDelay: Numeric;
  jitterBufferEmittedCount: Numeric;
  packetsLost: Numeric;
  nackCount: Numeric;
  pliCount: Numeric;
  freezeCount: Numeric;
  totalFreezesDuration: Numeric;
}
interface PresentedFrame {
  atMs: number;
  presentedFrames: number;
  mediaTime: number;
  processingDuration: Numeric;
}
interface Measurement {
  startedAtMs: number;
  finishedAtMs: number;
  samples: MediaSample[];
  frames: PresentedFrame[];
  overflow: number;
  width: number;
  height: number;
}

let peer: RTCPeerConnection | null = null;
let frameCallback: number | null = null;
let timer: ReturnType<typeof setTimeout> | null = null;
let measurement: Measurement | null = null;
let active: Measurement | null = null;
let latest: MediaSample | null = null;
const capabilities = RTCRtpReceiver.getCapabilities("video");

function numeric(record: Record<string, unknown>, key: string): Numeric {
  const value = record[key];
  return typeof value === "number" && Number.isFinite(value) ? value : null;
}
function text(record: Record<string, unknown>, key: string): string | null {
  const value = record[key];
  return typeof value === "string" ? value : null;
}
async function sample(): Promise<void> {
  const connection = peer;
  if (!connection) return;
  const report = await connection.getStats();
  if (connection !== peer) return;
  report.forEach((value: Record<string, unknown>) => {
    if (value["type"] !== "inbound-rtp" || value["kind"] !== "video") return;
    const codecId = text(value, "codecId");
    const codec: Record<string, unknown> | undefined = codecId
      ? report.get(codecId)
      : undefined;
    latest = {
      atMs: performance.now(),
      codec: codec ? text(codec, "mimeType") : null,
      fmtp: codec ? text(codec, "sdpFmtpLine") : null,
      decoderImplementation: text(value, "decoderImplementation"),
      powerEfficientDecoder:
        typeof value["powerEfficientDecoder"] === "boolean"
          ? value["powerEfficientDecoder"]
          : null,
      framesDecoded: numeric(value, "framesDecoded"),
      framesDropped: numeric(value, "framesDropped"),
      framesPerSecond: numeric(value, "framesPerSecond"),
      bytesReceived: numeric(value, "bytesReceived"),
      totalDecodeTime: numeric(value, "totalDecodeTime"),
      totalProcessingDelay: numeric(value, "totalProcessingDelay"),
      jitterBufferDelay: numeric(value, "jitterBufferDelay"),
      jitterBufferEmittedCount: numeric(value, "jitterBufferEmittedCount"),
      packetsLost: numeric(value, "packetsLost"),
      nackCount: numeric(value, "nackCount"),
      pliCount: numeric(value, "pliCount"),
      freezeCount: numeric(value, "freezeCount"),
      totalFreezesDuration: numeric(value, "totalFreezesDuration"),
    };
    if (active) {
      if (active.samples.length < 40) active.samples.push(latest);
      else active.overflow += 1;
    }
    statsElement.textContent = JSON.stringify(latest, null, 2);
  });
}
function observePresentation(
  now: number,
  metadata: VideoFrameCallbackMetadata,
): void {
  if (active) {
    if (active.frames.length < 3600)
      active.frames.push({
        atMs: now,
        presentedFrames: metadata.presentedFrames,
        mediaTime: metadata.mediaTime,
        processingDuration: metadata.processingDuration ?? null,
      });
    else active.overflow += 1;
  }
  frameCallback = peer
    ? video.requestVideoFrameCallback(observePresentation)
    : null;
}
async function poll(): Promise<void> {
  try {
    await sample();
  } catch (error) {
    status.textContent = `Stats failed: ${String(error)}`;
  }
  if (peer)
    timer = setTimeout(() => {
      void poll();
    }, 1000);
}
function stop(): void {
  releaseInput();
  controlSocket?.close();
  controlSocket = null;
  controlActive = false;
  const connection = peer;
  peer = null;
  if (timer !== null) clearTimeout(timer);
  if (frameCallback !== null) video.cancelVideoFrameCallback(frameCallback);
  connection?.close();
  video.srcObject = null;
  stopButton.disabled = true;
}
async function connect(): Promise<void> {
  connectButton.disabled = true;
  const configResponse = await fetch("/config", {
    signal: AbortSignal.timeout(5000),
  });
  if (!configResponse.ok)
    throw new Error("Could not read the test configuration.");
  const config: {
    source: string;
    iceServers: RTCIceServer[];
    control: boolean;
  } = await configResponse.json();
  document.querySelector("#description")!.textContent =
    config.source === "desktop"
      ? "Live Sprite desktop over WebRTC. Click the video to use the mouse and keyboard. Clipboard, pointer lock and automatic resizing are not included in this test."
      : "Local synthetic video only. No desktop control or live-service changes.";
  const connection = new RTCPeerConnection({ iceServers: config.iceServers });
  peer = connection;
  const transceiver = connection.addTransceiver("video", {
    direction: "recvonly",
  });
  const vp9 =
    capabilities?.codecs.filter(
      (codec) =>
        codec.mimeType.toLowerCase() === "video/vp9" &&
        codec.sdpFmtpLine === "profile-id=1",
    ) ?? [];
  if (vp9.length === 0)
    throw new Error(
      "Chrome did not advertise a VP9 profile 1 WebRTC receiver.",
    );
  transceiver.setCodecPreferences(vp9);
  connection.ontrack = (event) => {
    video.srcObject = new MediaStream([event.track]);
    void video.play().catch((error: unknown) => {
      if (peer === connection) status.textContent = String(error);
    });
  };
  connection.onconnectionstatechange = () => {
    status.textContent = connection.connectionState;
    if (connection.connectionState === "failed") {
      stop();
      status.textContent =
        "The video connection failed. No working network route was found.";
    }
  };
  const offer = await connection.createOffer();
  // Send the native description once it has a public candidate. Chrome may
  // keep checking other interfaces long after finding a usable address.
  await new Promise<void>((resolve, reject) => {
    const deadline = setTimeout(
      () => finish(new Error("Browser could not find a network address.")),
      10_000,
    );
    function finish(error?: unknown) {
      clearTimeout(deadline);
      connection.removeEventListener("icecandidate", candidate);
      if (error) reject(error);
      else resolve();
    }
    function candidate(event: RTCPeerConnectionIceEvent) {
      if (
        config.iceServers.length === 0
          ? event.candidate === null
          : event.candidate?.type === "srflx"
      )
        finish();
    }
    connection.addEventListener("icecandidate", candidate);
    void connection.setLocalDescription(offer).catch(finish);
  });
  if (peer !== connection) throw new Error("Connection was stopped.");
  const response = await fetch("/offer", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(connection.localDescription),
    signal: AbortSignal.timeout(15_000),
  });
  if (!response.ok) throw new Error(`Offer rejected: HTTP ${response.status}`);
  const answer: RTCSessionDescriptionInit = await response.json();
  if (peer !== connection) throw new Error("Connection was stopped.");
  await connection.setRemoteDescription(answer);
  if (config.control) openControl();
  stopButton.disabled = false;
  frameCallback = video.requestVideoFrameCallback(observePresentation);
  void poll();
}
connectButton.addEventListener("click", () => {
  void connect().catch((error: unknown) => {
    stop();
    status.textContent = String(error);
  });
});
stopButton.addEventListener("click", stop);
window.addEventListener("pagehide", stop);

const probe = {
  capabilities,
  get connected() {
    return peer?.connectionState === "connected" && video.videoWidth > 0;
  },
  get latest() {
    return latest;
  },
  get measurement() {
    return measurement;
  },
  startMeasurement(): void {
    if (!this.connected || active || measurement)
      throw new Error("One measurement requires connected video.");
    active = {
      startedAtMs: performance.now(),
      finishedAtMs: 0,
      samples: [],
      frames: [],
      overflow: 0,
      width: video.videoWidth,
      height: video.videoHeight,
    };
    setTimeout(() => {
      if (!active) return;
      active.finishedAtMs = performance.now();
      measurement = active;
      active = null;
    }, 30_000);
  },
  capturePng(): string {
    if (active)
      throw new Error("Pixel readback is forbidden during measurement.");
    if (!this.connected) throw new Error("No connected video.");
    const canvas = document.createElement("canvas");
    canvas.width = video.videoWidth;
    canvas.height = video.videoHeight;
    const context = canvas.getContext("2d");
    if (!context) throw new Error("Canvas context unavailable.");
    context.drawImage(video, 0, 0);
    return canvas.toDataURL("image/png");
  },
};
let controlSocket: WebSocket | null = null;
let controlActive = false;
let inputSequence = 0;
const controlStatus = document.querySelector<HTMLElement>("#control-status")!;
// Linux evdev codes, as used by the existing Waymote-derived input protocol.
const keys: Record<string, number> = {
  Escape: 1,
  Digit1: 2,
  Digit2: 3,
  Digit3: 4,
  Digit4: 5,
  Digit5: 6,
  Digit6: 7,
  Digit7: 8,
  Digit8: 9,
  Digit9: 10,
  Digit0: 11,
  Minus: 12,
  Equal: 13,
  Backspace: 14,
  Tab: 15,
  KeyQ: 16,
  KeyW: 17,
  KeyE: 18,
  KeyR: 19,
  KeyT: 20,
  KeyY: 21,
  KeyU: 22,
  KeyI: 23,
  KeyO: 24,
  KeyP: 25,
  BracketLeft: 26,
  BracketRight: 27,
  Enter: 28,
  ControlLeft: 29,
  KeyA: 30,
  KeyS: 31,
  KeyD: 32,
  KeyF: 33,
  KeyG: 34,
  KeyH: 35,
  KeyJ: 36,
  KeyK: 37,
  KeyL: 38,
  Semicolon: 39,
  Quote: 40,
  Backquote: 41,
  ShiftLeft: 42,
  Backslash: 43,
  KeyZ: 44,
  KeyX: 45,
  KeyC: 46,
  KeyV: 47,
  KeyB: 48,
  KeyN: 49,
  KeyM: 50,
  Comma: 51,
  Period: 52,
  Slash: 53,
  ShiftRight: 54,
  NumpadMultiply: 55,
  AltLeft: 56,
  Space: 57,
  CapsLock: 58,
  F1: 59,
  F2: 60,
  F3: 61,
  F4: 62,
  F5: 63,
  F6: 64,
  F7: 65,
  F8: 66,
  F9: 67,
  F10: 68,
  NumLock: 69,
  ScrollLock: 70,
  Numpad7: 71,
  Numpad8: 72,
  Numpad9: 73,
  NumpadSubtract: 74,
  Numpad4: 75,
  Numpad5: 76,
  Numpad6: 77,
  NumpadAdd: 78,
  Numpad1: 79,
  Numpad2: 80,
  Numpad3: 81,
  Numpad0: 82,
  NumpadDecimal: 83,
  IntlBackslash: 86,
  F11: 87,
  F12: 88,
  NumpadEnter: 96,
  ControlRight: 97,
  NumpadDivide: 98,
  PrintScreen: 99,
  AltRight: 100,
  Home: 102,
  ArrowUp: 103,
  PageUp: 104,
  ArrowLeft: 105,
  ArrowRight: 106,
  End: 107,
  ArrowDown: 108,
  PageDown: 109,
  Insert: 110,
  Delete: 111,
  Pause: 119,
  MetaLeft: 125,
  MetaRight: 126,
  ContextMenu: 127,
};
function sendRecord(
  kind: number,
  state: number,
  a = 0,
  b = 0,
  floats = false,
): void {
  if (!controlActive || controlSocket?.readyState !== WebSocket.OPEN) return;
  if (controlSocket.bufferedAmount > 65536) {
    controlSocket.close();
    controlActive = false;
    return;
  }
  const bytes = new ArrayBuffer(16);
  const view = new DataView(bytes);
  view.setUint8(0, 2);
  view.setUint8(1, kind);
  view.setUint8(2, state);
  if (floats) {
    view.setFloat32(4, a, true);
    view.setFloat32(8, b, true);
  } else {
    view.setUint32(4, a, true);
    view.setUint32(8, b, true);
  }
  if (kind !== 5) {
    inputSequence = (inputSequence + 1) >>> 0 || 1;
    view.setUint32(12, inputSequence, true);
  }
  controlSocket.send(bytes);
}
function releaseInput(): void {
  sendRecord(5, 0);
}
function openControl(): void {
  const socket = new WebSocket(
    `${location.protocol === "https:" ? "wss" : "ws"}://${location.host}/control`,
  );
  controlSocket = socket;
  socket.onopen = () => {
    socket.send("acquire");
  };
  socket.onmessage = (event: MessageEvent<unknown>) => {
    if (typeof event.data !== "string" || event.data.length > 65536) return;
    try {
      const message = JSON.parse(event.data) as {
        type?: string;
        state?: string;
      };
      if (message.type === "control-state") {
        controlActive = message.state === "active";
        controlStatus.textContent = controlActive
          ? "Mouse and keyboard ready"
          : `Control: ${message.state}`;
      }
    } catch {
      controlStatus.textContent = "Invalid control response";
      socket.close();
    }
  };
  socket.onclose = () => {
    controlActive = false;
    controlStatus.textContent = "Control disconnected";
  };
}
function movePointer(event: PointerEvent): void {
  const bounds = video.getBoundingClientRect();
  if (
    bounds.width <= 0 ||
    bounds.height <= 0 ||
    video.videoWidth <= 0 ||
    video.videoHeight <= 0
  )
    return;
  const scale = Math.min(
    bounds.width / video.videoWidth,
    bounds.height / video.videoHeight,
  );
  const width = video.videoWidth * scale;
  const height = video.videoHeight * scale;
  const left = bounds.left + (bounds.width - width) / 2;
  const top = bounds.top + (bounds.height - height) / 2;
  const x = Math.round(
    Math.max(0, Math.min(1, (event.clientX - left) / width)) * 65535,
  );
  const y = Math.round(
    Math.max(0, Math.min(1, (event.clientY - top) / height)) * 65535,
  );
  sendRecord(1, 0, x, y);
}
document.querySelector("#fullscreen")!.addEventListener("click", () => {
  void video
    .requestFullscreen()
    .then(() => video.focus())
    .catch((error: unknown) => {
      status.textContent = String(error);
    });
});
video.tabIndex = 0;
video.addEventListener("pointermove", movePointer);
video.addEventListener("pointerdown", (event) => {
  if (!controlActive) return;
  event.preventDefault();
  video.focus();
  video.setPointerCapture(event.pointerId);
  movePointer(event);
  const button = [0x110, 0x112, 0x111, 0x113, 0x114][event.button];
  if (button !== undefined) sendRecord(2, 1, button);
});
video.addEventListener("pointerup", (event) => {
  const button = [0x110, 0x112, 0x111, 0x113, 0x114][event.button];
  if (button !== undefined) sendRecord(2, 0, button);
});
video.addEventListener("pointercancel", releaseInput);
video.addEventListener("contextmenu", (event) => event.preventDefault());
video.addEventListener(
  "wheel",
  (event) => {
    if (!controlActive) return;
    event.preventDefault();
    const unit =
      event.deltaMode === 1
        ? 16
        : event.deltaMode === 2
          ? video.clientHeight
          : 1;
    sendRecord(
      3,
      0,
      Math.max(-4096, Math.min(4096, event.deltaX * unit)),
      Math.max(-4096, Math.min(4096, event.deltaY * unit)),
      true,
    );
  },
  { passive: false },
);
for (const type of ["keydown", "keyup"] as const)
  video.addEventListener(type, (event) => {
    if (!controlActive || event.isComposing) return;
    const key = keys[event.code];
    if (key === undefined) return;
    event.preventDefault();
    sendRecord(4, type === "keyup" ? 0 : event.repeat ? 2 : 1, key);
  });
video.addEventListener("blur", releaseInput);
window.addEventListener("blur", releaseInput);
Object.assign(window, { webrtcProbe: probe });
