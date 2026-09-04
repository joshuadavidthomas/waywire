// Recorder wiring check only. These idle/resize samples are NOT a performance comparison.
import assert from "node:assert/strict";
import { execFile } from "node:child_process";
import { mkdir, writeFile } from "node:fs/promises";
import { promisify } from "node:util";
import { setTimeout as sleep } from "node:timers/promises";
import { createAcceptanceProxy } from "../view-runtime.ts";

const exec = promisify(execFile);
const token = process.env.SPRITES_TOKEN;
assert(token, "SPRITES_TOKEN is required and stays in this process");
let browserSession = "";
async function browser(...args: string[]) {
  const { stdout } = await exec(
    "agent-browser",
    ["--session", browserSession, "--json", ...args],
    { timeout: 45_000 },
  );
  const reply = JSON.parse(stdout);
  assert(reply.success, JSON.stringify(reply.error));
  return reply.data;
}

await mkdir("probes/compare/results", { recursive: true });
for (const transport of ["waymote", "vnc"] as const) {
  browserSession = `desktop-comparison-${transport}`;
  const origin =
    transport === "waymote"
      ? "https://sprite-desktop-waymote-6ra.sprites.app"
      : "https://sprite-desktop-v1-conflicts-6ra.sprites.app";
  const proxy = createAcceptanceProxy(origin, token, transport);
  await new Promise<void>((resolve) =>
    proxy.server.listen(3217, "127.0.0.1", resolve),
  );
  try {
    await browser("open", "http://127.0.0.1:3217/?record");
    await browser(
      "wait",
      "--fn",
      transport === "waymote"
        ? 'document.querySelector("#status.connected") !== null'
        : 'document.querySelector("[data-first-frame-ms]") !== null',
    );
    await browser("set", "viewport", "1280", "900");
    await browser("snapshot", "-i");
    if (transport === "waymote") {
      // Acquire through the viewer control; no desktop typing/click injection.
      await browser(
        "find",
        "role",
        "button",
        "click",
        "--name",
        "Text input",
        "--exact",
      );
    }
    const fitsCanvas = `(() => {
      const c = document.querySelector('canvas');
      if (!c) return false;
      const r = c.getBoundingClientRect();
      const parent = c.parentElement.getBoundingClientRect();
      return c.width > 320 && Math.abs(c.width-r.width) < 32 && Math.abs(c.height-r.height) < 32
        && r.top >= parent.top && r.bottom <= parent.bottom + 1 && r.right <= parent.right + 1;
    })()`;
    await browser("wait", "--fn", fitsCanvas);
    await browser("snapshot", "-i");
    await browser(
      "find",
      "role",
      "button",
      "click",
      "--name",
      "Record 30 seconds",
    );
    await sleep(2000);
    // Both remote framebuffers must follow the browser's larger canvas.
    await browser("set", "viewport", "1600", "1000");
    await browser("wait", "--fn", fitsCanvas);
    await sleep(2000);
    await browser("snapshot", "-i");
    await browser(
      "find",
      "role",
      "button",
      "click",
      "--name",
      "Stop",
      "--exact",
    );
    await browser(
      "eval",
      "window.captureBlob = null; window.originalObjectURL = URL.createObjectURL; URL.createObjectURL = function(blob) { window.captureBlob = blob; return window.originalObjectURL(blob); }",
    );
    await browser("snapshot", "-i");
    await browser("find", "role", "button", "click", "--name", "Download JSON");
    const { result } = await browser(
      "eval",
      "window.captureBlob.text().then(JSON.parse)",
    );
    assert.equal(result.transport, transport);
    assert(result.durationMs > 1000 && result.durationMs < 30_000);
    assert(result.canvasSubmitsMs.length > 0, "Remote resize was not captured");
    assert(result.finalDimensions.width > result.dimensions.width + 200);
    assert(result.finalDimensions.height > result.dimensions.height);
    assert(result.receivedBytes > 0, "No incoming payload was counted");
    assert(result.animationFramesMs.length > 0);
    assert(result.canvasUpdateFramesMs.length <= result.canvasSubmitsMs.length);
    if (result.canvasUpdateFramesMs.length < 2)
      assert(result.summary.warnings.length > 0);
    assert(result.warnings.some((warning: string) => warning.includes("size")));
    await browser("screenshot", `/tmp/${transport}-comparison-responsive.png`);
    await writeFile(
      `probes/compare/results/${transport}-smoke.json`,
      JSON.stringify(
        {
          purpose:
            "Recorder wiring only; idle/resize workload, agent browser through local proxy. Not comparable performance evidence.",
          ...result,
        },
        null,
        2,
      ) + "\n",
    );
    console.log(
      transport,
      result.dimensions,
      result.finalDimensions,
      result.summary,
    );
  } finally {
    await browser("close").catch(() => {});
    proxy.revokeCredential();
    proxy.server.closeAllConnections();
    await new Promise<void>((resolve) => proxy.server.close(() => resolve()));
  }
}
