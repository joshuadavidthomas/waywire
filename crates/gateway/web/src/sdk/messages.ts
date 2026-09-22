// Mirrors PROTOCOL_VERSION in crates/protocol/src/lib.rs.
export const PROTOCOL_VERSION = 9;

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

const cursorShapeNames = [
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
] as const;

export type CursorShape = (typeof cursorShapeNames)[number];

const cursorShapes: ReadonlySet<string> = new Set(cursorShapeNames);

function isCursorShape(value: unknown): value is CursorShape {
  return typeof value === "string" && cursorShapes.has(value);
}

export type CursorState = {
  readonly type: "cursor";
  readonly visible: boolean;
  readonly shape: CursorShape;
  // Null until the server has sent its first normalized 0..65535 position.
  readonly position: { readonly x: number; readonly y: number } | null;
};

function isPointerCoordinate(value: unknown): value is number {
  return (
    typeof value === "number" &&
    Number.isInteger(value) &&
    value >= 0 &&
    value <= 65_535
  );
}

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
  | {
      readonly type: "reset-video-refused";
      readonly reason:
        | "current-mode-unknown"
        | "output-too-small"
        | "compositor-rejected"
        | "compositor-cancelled"
        | "compositor-timed-out"
        | "output-unavailable";
    }
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
    case "reset-video-refused":
      return value.reason === "current-mode-unknown" ||
        value.reason === "output-too-small" ||
        value.reason === "compositor-rejected" ||
        value.reason === "compositor-cancelled" ||
        value.reason === "compositor-timed-out" ||
        value.reason === "output-unavailable"
        ? { type: value.type, reason: value.reason }
        : null;
    case "cursor": {
      if (typeof value.visible !== "boolean" || !isCursorShape(value.shape))
        return null;
      if (value.position === undefined || value.position === null) {
        return {
          type: value.type,
          visible: value.visible,
          shape: value.shape,
          position: null,
        };
      }
      return isRecord(value.position) &&
        isPointerCoordinate(value.position.x) &&
        isPointerCoordinate(value.position.y)
        ? {
            type: value.type,
            visible: value.visible,
            shape: value.shape,
            position: { x: value.position.x, y: value.position.y },
          }
        : null;
    }
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
