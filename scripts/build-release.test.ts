import assert from "node:assert/strict";
import { execFile } from "node:child_process";
import { createHash, randomBytes } from "node:crypto";
import { mkdtemp, readFile, rm, stat } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import test from "node:test";
import { promisify } from "node:util";

const exec = promisify(execFile);
const root = resolve(import.meta.dirname, "..");

test(
  "release archive contains the one Socket runtime and a matching installer",
  { timeout: 180_000 },
  async () => {
    const version = `v0.0.0-test.${randomBytes(8).toString("hex")}`;
    const output = join(root, "dist/releases", version);
    const unpacked = await mkdtemp(join(tmpdir(), "socket-release-test-"));
    try {
      await exec(
        "pnpm",
        [
          "exec",
          "tsx",
          "scripts/build-release.ts",
          "--version",
          version,
          "--source",
          "local-contract-test",
          "--base-url",
          "https://example.invalid/releases",
        ],
        { cwd: root, timeout: 150_000, maxBuffer: 4 << 20 },
      );
      const archiveName = `sprite-desktop-${version}-linux-amd64.tar.gz`;
      const archive = join(output, archiveName);
      const digest = createHash("sha256")
        .update(await readFile(archive))
        .digest("hex");
      assert.equal(
        await readFile(join(output, "SHA256SUMS"), "utf8"),
        `${digest}  ${archiveName}\n`,
      );
      const installer = await readFile(join(output, "install.sh"), "utf8");
      assert(installer.includes(`readonly ARCHIVE_SHA256='${digest}'`));
      assert(
        installer.includes(
          `readonly ARCHIVE_SIZE='${(await stat(archive)).size}'`,
        ),
      );
      assert(!installer.includes("@SPRITE_DESKTOP_"));
      assert(
        installer.includes("/sys/fs/cgroup/svc.sprite-desktop/cgroup.procs"),
      );
      assert(installer.includes('sha256sum "/proc/$pid/exe"'));
      assert(installer.includes('"$candidate_pid" = "$old_service_pid"'));
      await exec("bash", ["-n", join(output, "install.sh")]);
      await exec("tar", ["-xzf", archive, "-C", unpacked]);
      const manifest = JSON.parse(
        await readFile(join(unpacked, "manifest.json"), "utf8"),
      );
      assert.deepEqual(manifest.artifacts, [
        "sprite-desktop-gateway",
        "sprite-desktop-streamd",
      ]);
      assert.deepEqual(manifest.ports, { http: 8080 });
      assert.deepEqual(manifest.services, [
        {
          name: "sprite-desktop",
          cmd: "/opt/sprite-desktop/current/bin/desktop.sh",
          args: [],
          http_port: 8080,
          needs: [],
          env: {},
          dir: "/home/sprite",
        },
      ]);
      for (const binary of manifest.artifacts) {
        const { stdout } = await exec(join(unpacked, "bin", binary), [
          "--version",
        ]);
        assert(stdout.startsWith(binary + " "));
      }
      const firstGateway = await readFile(
        join(unpacked, "bin/sprite-desktop-gateway"),
      );
      await exec("pnpm", ["build"], {
        cwd: root,
        timeout: 60_000,
        maxBuffer: 4 << 20,
      });
      assert.equal(
        createHash("sha256")
          .update(
            await readFile(join(root, "target/release/sprite-desktop-gateway")),
          )
          .digest("hex"),
        createHash("sha256").update(firstGateway).digest("hex"),
        "rebuilding identical viewer assets must not change the gateway binary",
      );
      const packagedSession = await readFile(
        join(unpacked, "bin/session.py"),
        "utf8",
      );
      assert.equal(
        packagedSession,
        await readFile(join(root, "installer/session.py"), "utf8"),
      );
      assert(packagedSession.includes('kwargs["start_new_session"] = True'));
      assert(packagedSession.includes("os.killpg(group, number)"));
      assert(packagedSession.includes("os.WNOWAIT"));
      const sources = JSON.parse(
        await readFile(join(unpacked, "sources.lock"), "utf8"),
      );
      const packages = sources.sources.flatMap(
        (source: { packages: string[] }) => source.packages,
      );
      for (const needed of [
        "labwc",
        "lxqt-core",
        "lxqt-wayland-session",
        "qt6-wayland",
        "ffmpeg",
        "grim",
        "python3",
      ]) {
        assert(packages.includes(needed), `missing runtime package ${needed}`);
      }
      assert(!packages.some((name: string) => /vnc|xfce|turn/iu.test(name)));
    } finally {
      await rm(unpacked, { recursive: true, force: true });
      await rm(output, { recursive: true, force: true });
    }
  },
);
