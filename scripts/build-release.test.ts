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
  "release archive contains the desktop runtime and a matching Sprite installer",
  { timeout: 180_000 },
  async () => {
    const version = `v0.0.0-test.${randomBytes(8).toString("hex")}`;
    const output = join(root, "dist/releases", version);
    const unpacked = await mkdtemp(join(tmpdir(), "socket-release-test-"));
    try {
      await exec("just", ["release", version, "local-contract-test"], {
        cwd: root,
        timeout: 150_000,
        maxBuffer: 4 << 20,
      });
      const archiveName = `waywire-${version}-linux-amd64.tar.gz`;
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
      assert(!installer.includes("@WAYWIRE_"));
      await exec("bash", ["-n", join(output, "install.sh")]);
      await exec("tar", ["-xzf", archive, "-C", unpacked]);
      const manifest = JSON.parse(
        await readFile(join(unpacked, "manifest.json"), "utf8"),
      );
      assert.deepEqual(manifest, {
        schema: 1,
        release: version,
        source: "local-contract-test",
        os: {
          id: "ubuntu",
          codename: "resolute",
          architecture: "amd64",
        },
        artifacts: ["waywire-gateway", "waywire-streamd"],
        ports: { http: 8080 },
      });
      for (const binary of manifest.artifacts) {
        const { stdout } = await exec(join(unpacked, "bin", binary), [
          "--version",
        ]);
        assert(stdout.startsWith(binary + " "));
      }
      for (const script of ["desktop.sh", "session.py"]) {
        assert.equal(
          await readFile(join(unpacked, "bin", script), "utf8"),
          await readFile(join(root, "desktop", script), "utf8"),
        );
      }
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
