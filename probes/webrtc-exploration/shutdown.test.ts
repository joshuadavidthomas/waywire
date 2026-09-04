import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { createConnection } from "node:net";
import { fileURLToPath } from "node:url";
import { setTimeout as sleep } from "node:timers/promises";
import test from "node:test";

const cwd = fileURLToPath(new URL("./", import.meta.url));

test(
  "shutdown bounds an unfinished HTTP offer body",
  { timeout: 15_000 },
  async () => {
    // A bind conflict makes this owned child exit; never touch the existing listener.
    const child = spawn(
      `${cwd}target/release/webrtc-exploration`,
      ["--source", "motion"],
      { cwd, stdio: ["ignore", "pipe", "pipe"] },
    );
    let log = "";
    child.stdout.on("data", (bytes: Buffer) => {
      log += bytes.toString();
    });
    child.stderr.on("data", (bytes: Buffer) => {
      log += bytes.toString();
    });
    const exited = new Promise<{
      code: number | null;
      signal: NodeJS.Signals | null;
    }>((resolve, reject) => {
      child.once("error", reject);
      child.once("exit", (code, signal) => resolve({ code, signal }));
    });
    let socket: ReturnType<typeof createConnection> | undefined;
    try {
      const deadline = Date.now() + 5000;
      while (!log.includes("listening on http://127.0.0.1:3220")) {
        assert(child.exitCode === null && child.signalCode === null, log);
        assert(Date.now() < deadline, "server did not become ready");
        await sleep(20);
      }
      socket = createConnection({ host: "127.0.0.1", port: 3220 });
      await new Promise<void>((resolve, reject) => {
        socket!.once("connect", resolve);
        socket!.once("error", reject);
      });
      socket.on("error", () => {}); // A forced drain may reset this owned connection.
      socket.write(
        "POST /offer HTTP/1.1\r\nHost: 127.0.0.1:3220\r\nOrigin: http://127.0.0.1:3220\r\nContent-Type: application/json\r\nContent-Length: 65536\r\n\r\n{",
      );
      await sleep(100);
      const started = Date.now();
      child.kill("SIGINT");
      const outcome = await Promise.race([
        exited,
        sleep(6000).then(() => null),
      ]);
      assert(outcome, "server waited indefinitely for the unfinished body");
      assert(Date.now() - started < 6000);
      assert.equal(outcome.signal, null);
      assert.equal(
        outcome.code,
        1,
        "a forced drain must report incomplete graceful shutdown",
      );
      assert(
        !log.includes("ffmpeg started"),
        "invalid incomplete offer must not spawn an encoder",
      );
    } finally {
      socket?.destroy();
      if (child.exitCode === null && child.signalCode === null)
        child.kill("SIGKILL");
      await exited;
    }
  },
);
