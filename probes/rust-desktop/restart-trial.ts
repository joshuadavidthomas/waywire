import assert from "node:assert/strict";
import type { Sprite } from "@fly/sprites";
import { performance } from "node:perf_hooks";
import { setTimeout as sleep } from "node:timers/promises";
import {
  assertDisposableTarget,
  sameTrialService,
  trialService,
  trialServiceName,
} from "./trial.js";

// Explicit benchmark preparation. This replaces only the owned service
// definition, never its files, so adaptation starts afresh with the same bytes.
export async function restartTrial(sprite: Sprite) {
  assertDisposableTarget(sprite.name);
  const existing = (await sprite.listServices()).find(
    (service) => service.name === trialServiceName,
  );
  assert(
    existing && sameTrialService(existing),
    "refusing to restart an unowned trial service",
  );
  // The deployed API has DELETE/PUT; its newer /restart endpoint is unavailable.
  await sprite.deleteService(trialServiceName);
  const logs = await sprite.createService(
    trialServiceName,
    {
      cmd: trialService.cmd,
      args: [...trialService.args],
      httpPort: trialService.httpPort,
      needs: [...trialService.needs],
      dir: trialService.dir,
    },
    "5s",
  );
  await logs.processAll(() => {});
  // A running service is not yet a ready encoder. Wait for its first keyframe,
  // then let native-check verify ownership and exact installed/live identity.
  const started = performance.now();
  let attempts = 0;
  while (performance.now() - started < 30_000) {
    attempts += 1;
    try {
      const result = await sprite.execFile(
        "curl",
        ["-fsS", "--max-time", "2", "http://127.0.0.1:8080/healthz"],
        { timeout: 5000 },
      );
      if (result.exitCode === 0 && result.stdout.toString() === "ok\n")
        return { healthWaitMs: performance.now() - started, attempts };
    } catch {
      // Connection refusal or 503 is expected while the new encoder starts.
    }
    await sleep(250);
  }
  throw new Error("restarted trial did not become healthy within 30 seconds");
}
