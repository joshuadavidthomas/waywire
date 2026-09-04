import { Effect, type Scope } from "effect";

import {
  WaymoteSession,
  type SurfaceHandle,
  type SurfaceOptions,
  type WaymoteSessionOptions,
} from "./waymote.ts";

export interface ManagedWaymoteSession {
  readonly session: WaymoteSession;
  readonly surface: SurfaceHandle;
}

export function acquireWaymoteSession(
  options: WaymoteSessionOptions,
  surfaceOptions: SurfaceOptions,
): Effect.Effect<ManagedWaymoteSession, Error, Scope.Scope> {
  return Effect.gen(function* () {
    const session = yield* Effect.acquireRelease(
      Effect.sync(() => new WaymoteSession(options)),
      (activeSession) => Effect.promise(() => activeSession.dispose()),
    );
    const surface = yield* Effect.acquireRelease(
      Effect.try({
        try: () => session.attachSurface(surfaceOptions),
        catch: (cause) =>
          cause instanceof Error ? cause : new Error(String(cause)),
      }),
      (attachedSurface) => Effect.sync(() => attachedSurface.dispose()),
    );
    yield* Effect.try({
      try: () => session.connect(),
      catch: (cause) =>
        cause instanceof Error ? cause : new Error(String(cause)),
    });
    return { session, surface };
  });
}
