import assert from "node:assert/strict";
import { spawn, execFile } from "node:child_process";
import { createHash } from "node:crypto";
import { mkdir, readFile, writeFile, open } from "node:fs/promises";
import { fileURLToPath } from "node:url";
import { promisify } from "node:util";
import { setTimeout as sleep } from "node:timers/promises";
import { startHostSampling } from "../rust-desktop/host-sampling.js";

const exec = promisify(execFile);
const cwd = fileURLToPath(new URL("./", import.meta.url));
const source = process.argv[2];
assert(source === "motion" || source === "chart", "usage: run.ts motion|chart");
const session = "rust-webrtc-exploration";
const directory = `${cwd}results/${source}-${new Date().toISOString().replaceAll(":", "-")}`;
await mkdir(directory, { recursive: true });
const result: Record<string, unknown> = {
  scope:
    "Local synthetic WebRTC/native-video proof, not Sprite acceptance or a matched CPU comparison. Encoder and browser share this host. No custom frame scheduling or timed pixel readback.",
  source,
  startedAt: new Date().toISOString(),
};
async function browser(
  command: string,
  ...args: string[]
): Promise<Record<string, unknown>> {
  const response = await exec(
    "agent-browser",
    ["--session", session, "--json", command, ...args],
    { timeout: 50_000, maxBuffer: 8 * 1024 * 1024 },
  );
  const json = JSON.parse(response.stdout) as {
    success: boolean;
    data: Record<string, unknown>;
    error?: unknown;
  };
  assert(json.success, JSON.stringify(json.error));
  return json.data;
}
async function evaluate<T>(expression: string): Promise<T> {
  return (
    await browser("eval", "-b", Buffer.from(expression).toString("base64"))
  )["result"] as T;
}
const ports = await exec("ss", ["-H", "-ltn", "sport = :3220"]);
assert.equal(
  ports.stdout.trim(),
  "",
  "refusing to use occupied prototype port",
);
let server: ReturnType<typeof spawn> | undefined;
let serverExit:
  | Promise<{ code: number | null; signal: NodeJS.Signals | null }>
  | undefined;
let sampler: Awaited<ReturnType<typeof startHostSampling>> | undefined;
let serverSampler: Awaited<ReturnType<typeof startHostSampling>> | undefined;
let failure: unknown;
let encoderIdentity:
  | { pid: number; parent: number; startTicks: string }
  | undefined;
async function identity(pid: number) {
  try {
    const stat = await readFile(`/proc/${pid}/stat`, "utf8");
    const fields = stat
      .slice(stat.lastIndexOf(")") + 2)
      .trim()
      .split(/\s+/u);
    return { pid, parent: Number(fields[1]), startTicks: fields[19]! };
  } catch (error) {
    if ((error as NodeJS.ErrnoException).code === "ENOENT") return null;
    throw error;
  }
}
async function cleanupStep(key: string, action: () => Promise<unknown>) {
  try {
    result[key] = await action();
  } catch (error) {
    failure ??= error;
    result[`${key}Error`] = String(error);
  }
}
async function stopEncoderIfPresent() {
  if (!encoderIdentity) return;
  const current = await identity(encoderIdentity.pid);
  if (!current || current.startTicks !== encoderIdentity.startTicks) return;
  process.kill(encoderIdentity.pid, "SIGKILL");
  const deadline = Date.now() + 5000;
  while (Date.now() < deadline) {
    const remaining = await identity(encoderIdentity.pid);
    if (!remaining || remaining.startTicks !== encoderIdentity.startTicks)
      return;
    await sleep(50);
  }
  throw new Error("Owned encoder still present after forced cleanup");
}
const log = await open(`${directory}/server.log`, "wx");
try {
  await browser("open", "about:blank");
  result["browser"] = await evaluate(
    "({ userAgent: navigator.userAgent, capabilities: RTCRtpReceiver.getCapabilities('video') })",
  );
  if (source === "chart") {
    const svg = await readFile(`${cwd}chart.svg`, "utf8");
    const uri = `data:image/svg+xml;base64,${Buffer.from(svg).toString("base64")}`;
    const image = await evaluate<string>(
      `(async()=>{const image=new Image();image.src=${JSON.stringify(uri)};await image.decode();const canvas=document.createElement('canvas');canvas.width=1824;canvas.height=848;const context=canvas.getContext('2d');context.drawImage(image,0,0);return canvas.toDataURL('image/png')})()`,
    );
    const bytes = Buffer.from(image.split(",")[1]!, "base64");
    await writeFile(`${cwd}chart.png`, bytes);
    await writeFile(`${directory}/source.png`, bytes, { flag: "wx" });
  }
  const executable = `${cwd}target/release/webrtc-exploration`;
  result["binarySha256"] = createHash("sha256")
    .update(await readFile(executable))
    .digest("hex");
  server = spawn(executable, ["--source", source], {
    cwd,
    stdio: ["ignore", log.fd, log.fd],
  });
  result["serverPid"] = server.pid;
  serverExit = new Promise((resolve, reject) => {
    server!.once("error", reject);
    server!.once("exit", (code, signal) => resolve({ code, signal }));
  });
  const start = Date.now();
  for (;;) {
    assert(
      server.exitCode === null && server.signalCode === null,
      "prototype exited before HTTP readiness",
    );
    try {
      const response = await fetch("http://127.0.0.1:3220", {
        signal: AbortSignal.timeout(500),
      });
      if (response.ok) break;
    } catch {
      /* listener not ready yet */
    }
    assert(Date.now() - start < 10_000, "HTTP readiness timed out");
    await sleep(100);
  }
  await browser("open", "http://127.0.0.1:3220");
  await browser("click", "#connect");
  await browser("wait", "--fn", "window.webrtcProbe.connected");
  const encoderMatch = /ffmpeg started \(pid (\d+),/u.exec(
    await readFile(`${directory}/server.log`, "utf8"),
  );
  assert(encoderMatch, "owned encoder PID was not reported");
  const ownedEncoder = await identity(Number(encoderMatch[1]));
  assert(
    ownedEncoder && ownedEncoder.parent === server.pid,
    "encoder ownership changed",
  );
  encoderIdentity = ownedEncoder;
  result["encoderIdentity"] = encoderIdentity;
  // Warmup is outside the measurement, allowing the receiver buffer to settle.
  await sleep(2000);
  const info = await browser("session", "info");
  assert(typeof info["pid"] === "number");
  sampler = await startHostSampling(info["pid"]);
  assert(server.pid);
  serverSampler = await startHostSampling(server.pid);
  await evaluate("window.webrtcProbe.startMeasurement()");
  await sleep(30_500);
  const measurement = await evaluate<{
    width: number;
    height: number;
    frames: unknown[];
    overflow: number;
  } | null>("window.webrtcProbe.measurement");
  result["measurement"] = measurement;
  assert(measurement, "measurement did not finish");
  assert.equal(measurement.width, 1824);
  assert.equal(measurement.height, 848);
  assert(measurement.frames.length > 0, "no video presentations observed");
  assert.equal(measurement.overflow, 0);
  assert(
    await evaluate<boolean>("window.webrtcProbe.connected"),
    "video connection ended during measurement",
  );
  result["latest"] = await evaluate("window.webrtcProbe.latest");
  result["host"] = await sampler.finish();
  sampler = undefined;
  result["serverHost"] = await serverSampler.finish();
  serverSampler = undefined;
  const captured = await evaluate<string>("window.webrtcProbe.capturePng()");
  await writeFile(
    `${directory}/decoded.png`,
    Buffer.from(captured.split(",")[1]!, "base64"),
    { flag: "wx" },
  );
  await browser("screenshot", `${directory}/browser.png`);
  if (source === "chart") {
    const psnr = await exec(
      "ffmpeg",
      [
        "-hide_banner",
        "-i",
        `${directory}/source.png`,
        "-i",
        `${directory}/decoded.png`,
        "-filter_complex",
        "[0:v]format=gbrp[a];[1:v]format=gbrp[b];[a][b]psnr",
        "-frames:v",
        "1",
        "-f",
        "null",
        "-",
      ],
      { timeout: 15_000 },
    );
    await writeFile(`${directory}/psnr.log`, psnr.stderr, { flag: "wx" });
    const match = /average:([0-9.]+|inf)/u.exec(psnr.stderr);
    assert(match, "PSNR output missing");
    result["rgbPsnrDb"] = match[1] === "inf" ? "infinity" : Number(match[1]);
    result["fidelityThresholdPassed"] =
      match[1] === "inf" || Number(match[1]) >= 35;
    assert(result["fidelityThresholdPassed"], "RGB fidelity is below 35 dB");
  }
  result["status"] = "PLAYBACK_OBSERVED";
} catch (error) {
  failure = error;
  result["status"] = "FAILED";
  result["error"] = String(error);
} finally {
  await cleanupStep("encoderTracked", async () => {
    if (!encoderIdentity && server?.pid) {
      const match = /ffmpeg started \(pid (\d+),/u.exec(
        await readFile(`${directory}/server.log`, "utf8"),
      );
      if (match) {
        const found = await identity(Number(match[1]));
        if (found?.parent === server.pid) encoderIdentity = found;
      }
    }
    return encoderIdentity ?? null;
  });
  if (sampler) await cleanupStep("host", () => sampler!.finish());
  if (serverSampler)
    await cleanupStep("serverHost", () => serverSampler!.finish());
  await cleanupStep("browserClosed", async () => {
    await browser("close");
    return true;
  });
  if (server && serverExit) {
    await cleanupStep("serverExit", async () => {
      if (server!.exitCode === null && server!.signalCode === null)
        server!.kill("SIGINT");
      let outcome = await Promise.race([
        serverExit!,
        sleep(10_000).then(() => null),
      ]);
      if (!outcome) {
        failure ??= new Error("Owned server exceeded cleanup deadline");
        result["forcedServerCleanup"] = true;
        // Kill the validated child before forcing its parent to exit.
        await cleanupStep("forcedEncoderCleanup", stopEncoderIfPresent);
        server!.kill("SIGKILL");
        outcome = await Promise.race([
          serverExit!,
          sleep(5000).then(() => null),
        ]);
      }
      result["serverExit"] = outcome;
      assert(outcome, "owned server did not exit after SIGKILL");
      assert.equal(outcome.code, 0, "prototype server exited unsuccessfully");
      return outcome;
    });
  }
  await cleanupStep("encoderAbsent", async () => {
    if (!encoderIdentity) return null;
    const remaining = await identity(encoderIdentity.pid);
    if (remaining?.startTicks === encoderIdentity.startTicks) {
      failure ??= new Error("Encoder outlived the server's cleanup");
      await stopEncoderIfPresent();
    }
    return true;
  });
  await cleanupStep("logClosed", () => log.close());
  await cleanupStep("portEmpty", async () => {
    const remaining = await exec("ss", ["-H", "-ltn", "sport = :3220"]);
    assert.equal(remaining.stdout.trim(), "", "prototype port remained open");
    return true;
  });
  if (failure) result["status"] = "FAILED";
  result["finishedAt"] = new Date().toISOString();
  await writeFile(
    `${directory}/result.json`,
    `${JSON.stringify(result, null, 2)}\n`,
    { flag: "wx" },
  );
  console.log(
    JSON.stringify(
      {
        directory,
        status: result["status"],
        latest: result["latest"],
        rgbPsnrDb: result["rgbPsnrDb"],
        serverExit: result["serverExit"],
        portEmpty: result["portEmpty"],
        error: result["error"],
      },
      null,
      2,
    ),
  );
}
if (failure) throw failure;
