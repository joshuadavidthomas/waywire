import { execFile } from "node:child_process";
import { createHash } from "node:crypto";
import {
  chmod,
  copyFile,
  mkdir,
  mkdtemp,
  readFile,
  rm,
  stat,
  writeFile,
} from "node:fs/promises";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { parseArgs, promisify } from "node:util";
import { valid } from "semver";

const exec = promisify(execFile);
const { values } = parseArgs({
  options: {
    version: { type: "string" },
    source: { type: "string" },
  },
  strict: true,
});

const version = values.version;
const source = values.source;
if (
  !version ||
  !/^v\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?$/u.test(version) ||
  !valid(version)
) {
  throw new Error("--version must be a v-prefixed semantic version");
}
if (!source || !/^[0-9A-Za-z][0-9A-Za-z._/-]{0,127}$/u.test(source)) {
  throw new Error("--source must name the release source revision");
}

const root = resolve(import.meta.dirname, "..");
const output = join(root, "dist", "releases", version);
const temporary = await mkdtemp(join(tmpdir(), "sprite-desktop-release-"));
const payload = join(temporary, "payload");
const archiveName = `sprite-desktop-${version}-linux-amd64.tar.gz`;
const archive = join(output, archiveName);
let createdOutput = false;
let completed = false;

const service = {
  name: "sprite-desktop",
  cmd: "/opt/sprite-desktop/current/bin/desktop.sh",
  args: [],
  http_port: 8080,
  needs: [],
  env: {},
  dir: "/home/sprite",
};
const packages = [
  "breeze-cursor-theme",
  "breeze-icon-theme",
  "dbus-x11",
  "featherpad",
  "ffmpeg",
  "labwc",
  "lxqt-core",
  "lxqt-menu-data",
  "lxqt-wayland-session",
  "qt6-svg-plugins",
  "qt6-wayland",
  "grim",
  "python3",
  "wayland-utils",
  "wlr-randr",
  "fonts-dejavu-core",
  "xdg-utils",
  "curl",
  "ca-certificates",
  "firefox",
];

try {
  await mkdir(join(root, "dist", "releases"), { recursive: true });
  await mkdir(output);
  createdOutput = true;
  await mkdir(join(payload, "bin"), { recursive: true });

  await exec("pnpm", ["--filter", "@sprite-desktop/web", "build"], {
    cwd: root,
  });
  await exec("cargo", ["build", "--locked", "--release", "--workspace"], {
    cwd: root,
  });
  await Promise.all([
    copyFile(
      join(root, "target/release/sprite-desktop-gateway"),
      join(payload, "bin/sprite-desktop-gateway"),
    ),
    copyFile(
      join(root, "target/release/sprite-desktop-streamd"),
      join(payload, "bin/sprite-desktop-streamd"),
    ),
    copyFile(
      join(root, "installer/desktop.sh"),
      join(payload, "bin/desktop.sh"),
    ),
    copyFile(
      join(root, "installer/session.py"),
      join(payload, "bin/session.py"),
    ),
  ]);
  await Promise.all(
    [
      "sprite-desktop-gateway",
      "sprite-desktop-streamd",
      "desktop.sh",
      "session.py",
    ].map((name) => chmod(join(payload, "bin", name), 0o555)),
  );

  const manifest = {
    schema: 1,
    release: version,
    source,
    os: { id: "ubuntu", codename: "resolute", architecture: "amd64" },
    artifacts: ["sprite-desktop-gateway", "sprite-desktop-streamd"],
    ports: { http: 8080 },
    services: [service],
  };
  const sources = {
    schema: 1,
    sources: [
      {
        name: "ubuntu",
        url: "https://archive.ubuntu.com/ubuntu",
        suites: ["resolute", "resolute-updates", "resolute-security"],
        components: ["main", "universe"],
        keyring: "/usr/share/keyrings/ubuntu-archive-keyring.gpg",
        key_fingerprints: ["F6ECB3762474EDA9D21B7022871920D1991BC93C"],
        packages: packages.filter((name) => name !== "firefox"),
      },
      {
        name: "mozilla",
        url: "https://packages.mozilla.org/apt",
        key_url: "https://packages.mozilla.org/apt/repo-signing-key.gpg",
        keyring: "/etc/apt/keyrings/packages.mozilla.org.asc",
        components: ["main"],
        key_fingerprints: ["35BAA0B33E9EB396F59CA838C0BA5CE6DC6315A3"],
        suites: ["mozilla"],
        packages: ["firefox"],
      },
    ],
  };
  await Promise.all([
    writeFile(
      join(payload, "manifest.json"),
      `${JSON.stringify(manifest, null, 2)}\n`,
      { mode: 0o444 },
    ),
    writeFile(
      join(payload, "sources.lock"),
      `${JSON.stringify(sources, null, 2)}\n`,
      { mode: 0o444 },
    ),
  ]);

  await exec("tar", [
    "-z",
    "--sort=name",
    "--mtime=@0",
    "--owner=0",
    "--group=0",
    "--numeric-owner",
    "-cf",
    archive,
    "-C",
    payload,
    ".",
  ]);
  const archiveBytes = await readFile(archive);
  const digest = createHash("sha256").update(archiveBytes).digest("hex");
  const size = (await stat(archive)).size;
  await writeFile(join(output, "SHA256SUMS"), `${digest}  ${archiveName}\n`);

  const template = await readFile(join(root, "installer/install.sh"), "utf8");
  const installer = template
    .replaceAll("@SPRITE_DESKTOP_VERSION@", version)
    .replaceAll("@SPRITE_DESKTOP_ARCHIVE_SIZE@", String(size))
    .replaceAll("@SPRITE_DESKTOP_ARCHIVE_SHA256@", digest);
  if (installer.includes("@SPRITE_DESKTOP_"))
    throw new Error("installer template has an unreplaced token");
  await writeFile(join(output, "install.sh"), installer, { mode: 0o755 });
  completed = true;
  console.log(`Built ${output}`);
  console.log(`${digest}  ${archiveName} (${size} bytes)`);
} finally {
  await rm(temporary, { recursive: true, force: true });
  if (createdOutput && !completed)
    await rm(output, { recursive: true, force: true });
}
