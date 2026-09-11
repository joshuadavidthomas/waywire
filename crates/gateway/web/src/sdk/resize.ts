export interface RemoteDisplayManualPolicy {
  readonly mode: "manual";
}

export interface RemoteDisplayFixedPolicy {
  readonly mode: "fixed";
  readonly width: number;
  readonly height: number;
  readonly scale: number;
}

export interface RemoteDisplayObservePolicy {
  readonly mode: "observe";
  readonly element?: Element;
  readonly devicePixelRatio?: number;
  readonly debounceMs?: number;
  readonly minWidth?: number;
  readonly minHeight?: number;
  readonly maxWidth?: number;
  readonly maxHeight?: number;
  readonly maxPixels?: number;
}

export type RemoteDisplayPolicy =
  | RemoteDisplayManualPolicy
  | RemoteDisplayFixedPolicy
  | RemoteDisplayObservePolicy;

export const automaticResizeAlignment = 2;
const minimumOutputWidth = 320;
const maximumOutputWidth = 6000;
const minimumOutputHeight = 180;
const maximumOutputHeight = 6000;
const maximumOutputPixels = maximumOutputWidth * maximumOutputHeight;
const defaultAutomaticOutputPixels = 3840 * 2160;

export function fitObservedResize(
  rawWidth: number,
  rawHeight: number,
  policy: Omit<RemoteDisplayObservePolicy, "mode"> = {},
): Readonly<{ width: number; height: number; downscale: number }> {
  const minWidth = Math.min(
    maximumOutputWidth,
    policy.minWidth ?? minimumOutputWidth,
  );
  const minHeight = Math.min(
    maximumOutputHeight,
    policy.minHeight ?? minimumOutputHeight,
  );
  const maxWidth = Math.max(
    minWidth,
    Math.min(maximumOutputWidth, policy.maxWidth ?? maximumOutputWidth),
  );
  const maxHeight = Math.max(
    minHeight,
    Math.min(maximumOutputHeight, policy.maxHeight ?? maximumOutputHeight),
  );
  const maxPixels = Math.min(
    maximumOutputPixels,
    policy.maxPixels ?? defaultAutomaticOutputPixels,
  );
  if (minWidth * minHeight > maxPixels) {
    throw new RangeError("Remote display minimum size exceeds maxPixels");
  }
  const downscale = Math.min(
    1,
    maxWidth / rawWidth,
    maxHeight / rawHeight,
    Math.sqrt(maxPixels / (rawWidth * rawHeight)),
  );
  // Match streamd's even dimensions. H.264 pads its own macroblocks; rounding
  // the desktop to 16 pixels needlessly turns 1080 lines into 1072.
  // sendResize separately suppresses small layout oscillations.
  const width = Math.max(
    minWidth,
    Math.min(
      maxWidth,
      Math.floor((rawWidth * downscale) / automaticResizeAlignment) *
        automaticResizeAlignment,
    ),
  );
  const height = Math.max(
    minHeight,
    Math.min(
      maxHeight,
      Math.floor((rawHeight * downscale) / automaticResizeAlignment) *
        automaticResizeAlignment,
    ),
  );
  return Object.freeze({ width, height, downscale });
}

export function normalizeResizeDimensions(
  width: number,
  height: number,
): Readonly<{ width: number; height: number }> {
  return Object.freeze({
    width: Math.max(
      minimumOutputWidth,
      Math.min(maximumOutputWidth, Math.round(width / 2) * 2),
    ),
    height: Math.max(
      minimumOutputHeight,
      Math.min(maximumOutputHeight, Math.round(height / 2) * 2),
    ),
  });
}

function positiveNumber(value: unknown, name: string): number {
  const number = Number(value);
  if (!Number.isFinite(number) || number <= 0) {
    throw new RangeError(`${name} must be a positive number`);
  }
  return number;
}

function optionalPositiveNumber(
  value: unknown,
  name: string,
): number | undefined {
  return value === undefined ? undefined : positiveNumber(value, name);
}

export function normalizeRemoteDisplayPolicy(
  policy: unknown,
): RemoteDisplayPolicy {
  if (typeof policy !== "object" || policy === null || !("mode" in policy)) {
    throw new TypeError("Remote display policy must be an object");
  }
  if (policy.mode === "manual") return Object.freeze({ mode: "manual" });
  if (
    policy.mode === "fixed" &&
    "width" in policy &&
    "height" in policy &&
    "scale" in policy
  ) {
    return Object.freeze({
      mode: "fixed",
      width: positiveNumber(policy.width, "Remote display width"),
      height: positiveNumber(policy.height, "Remote display height"),
      scale: positiveNumber(policy.scale, "Remote display scale"),
    });
  }
  if (policy.mode !== "observe") {
    throw new TypeError(`Unknown remote display mode: ${String(policy.mode)}`);
  }
  const element = "element" in policy ? policy.element : undefined;
  if (element !== undefined && !(element instanceof Element)) {
    throw new TypeError(
      "Observed remote display element must be a DOM element",
    );
  }
  const debounceValue = "debounceMs" in policy ? policy.debounceMs : undefined;
  const debounceMs =
    debounceValue === undefined ? undefined : Number(debounceValue);
  if (
    debounceMs !== undefined &&
    (!Number.isFinite(debounceMs) || debounceMs < 0)
  ) {
    throw new RangeError("debounceMs must be a non-negative number");
  }
  const normalized: RemoteDisplayObservePolicy = {
    mode: "observe",
    ...(element === undefined ? {} : { element }),
    ...(debounceMs === undefined ? {} : { debounceMs }),
    ...optionalPolicyNumber(policy, "devicePixelRatio"),
    ...optionalPolicyNumber(policy, "minWidth"),
    ...optionalPolicyNumber(policy, "minHeight"),
    ...optionalPolicyNumber(policy, "maxWidth"),
    ...optionalPolicyNumber(policy, "maxHeight"),
    ...optionalPolicyNumber(policy, "maxPixels"),
  };
  return Object.freeze(normalized);
}

function optionalPolicyNumber<K extends keyof RemoteDisplayObservePolicy>(
  policy: object,
  name: K,
): Partial<Pick<RemoteDisplayObservePolicy, K>> {
  if (!(name in policy)) return {};
  const value = optionalPositiveNumber(Reflect.get(policy, name), String(name));
  return value === undefined
    ? {}
    : ({ [name]: value } as Partial<Pick<RemoteDisplayObservePolicy, K>>);
}
