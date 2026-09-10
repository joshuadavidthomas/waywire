// Mirrors PROTOCOL_VERSION in crates/protocol/src/lib.rs.
export const PROTOCOL_VERSION = 2;

export class ProtocolVersionMismatchError extends Error {
  constructor(
    readonly expected: number,
    readonly actual: number,
  ) {
    super(
      `server speaks protocol version ${actual}, this page speaks ${expected}`,
    );
    this.name = "ProtocolVersionMismatchError";
  }
}

export type CursorBitmap = {
  readonly width: number;
  readonly height: number;
  readonly hotspot: {
    readonly x: number;
    readonly y: number;
  };
  readonly image: string;
};

export type CursorState = {
  readonly type: "cursor";
  readonly visible: boolean;
  // Null until the server has sent its first cursor image.
  readonly bitmap: CursorBitmap | null;
};

export type VideoConfiguration = {
  readonly type: "video-config";
  readonly version: number;
  readonly codec: string;
};

export type ControlMessage =
  | {
      readonly type: "control-state";
      readonly state: "active" | "busy" | "ready";
    }
  | {
      readonly type: "pong";
      readonly id: number;
      readonly serverNanos: string | number;
    }
  | {
      readonly type: "resize-applied";
      readonly request: number;
      readonly width: number;
      readonly height: number;
      readonly scale: number;
      readonly generation: number;
    }
  | CursorState
  | { readonly type: "clipboard"; readonly text: string }
  | {
      readonly type: "quality";
      readonly bitrate: number;
      readonly fps: number;
      readonly scale: number;
    };

export function parseJson(text: string): unknown {
  try {
    return JSON.parse(text);
  } catch {
    return null;
  }
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

export function parseControlMessage(value: unknown): ControlMessage | null {
  if (!isRecord(value)) return null;
  switch (value.type) {
    case "control-state":
      return value.state === "active" ||
        value.state === "busy" ||
        value.state === "ready"
        ? { type: value.type, state: value.state }
        : null;
    case "pong":
      return typeof value.id === "number" &&
        (typeof value.serverNanos === "string" ||
          typeof value.serverNanos === "number")
        ? { type: value.type, id: value.id, serverNanos: value.serverNanos }
        : null;
    case "resize-applied":
      return typeof value.request === "number" &&
        typeof value.width === "number" &&
        typeof value.height === "number" &&
        typeof value.scale === "number" &&
        typeof value.generation === "number"
        ? {
            type: value.type,
            request: value.request,
            width: value.width,
            height: value.height,
            scale: value.scale,
            generation: value.generation,
          }
        : null;
    case "cursor":
      if (typeof value.visible !== "boolean") return null;
      if (value.image === undefined) {
        return { type: value.type, visible: value.visible, bitmap: null };
      }
      return typeof value.width === "number" &&
        typeof value.height === "number" &&
        isRecord(value.hotspot) &&
        typeof value.hotspot.x === "number" &&
        typeof value.hotspot.y === "number" &&
        typeof value.image === "string"
        ? {
            type: value.type,
            visible: value.visible,
            bitmap: {
              width: value.width,
              height: value.height,
              hotspot: { x: value.hotspot.x, y: value.hotspot.y },
              image: value.image,
            },
          }
        : null;
    case "clipboard":
      return typeof value.text === "string"
        ? { type: value.type, text: value.text }
        : null;
    case "quality":
      return typeof value.bitrate === "number" &&
        typeof value.fps === "number" &&
        typeof value.scale === "number"
        ? {
            type: value.type,
            bitrate: value.bitrate,
            fps: value.fps,
            scale: value.scale,
          }
        : null;
    default:
      return null;
  }
}

export function parseVideoConfiguration(
  value: unknown,
): VideoConfiguration | null {
  return isRecord(value) &&
    value.type === "video-config" &&
    typeof value.version === "number" &&
    typeof value.codec === "string"
    ? { type: value.type, version: value.version, codec: value.codec }
    : null;
}
