import { execFile } from "node:child_process";
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
import { createHash } from "node:crypto";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";
import { parseArgs, promisify } from "node:util";
import { valid } from "semver";

const exec = promisify(execFile);
const { values } = parseArgs({
  options: {
    version: { type: "string" },
    source: { type: "string" },
    "base-url": { type: "string" },
  },
  strict: true,
});

const version = values.version;
const source = values.source;
const baseURL = values["base-url"];
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
if (!baseURL) throw new Error("--base-url is required");
const parsedBaseURL = new URL(baseURL);
if (
  parsedBaseURL.protocol !== "https:" ||
  parsedBaseURL.username ||
  parsedBaseURL.password ||
  parsedBaseURL.search ||
  parsedBaseURL.hash ||
  baseURL.includes("'")
) {
  throw new Error(
    "--base-url must be an HTTPS release directory without credentials, a query, fragment, or single quote",
  );
}

const root = resolve(import.meta.dirname, "..");
const output = join(root, "dist", "releases", version);
const temporary = await mkdtemp(join(tmpdir(), "sprite-desktop-release-"));
const payload = join(temporary, "payload");
const archiveName = `sprite-desktop-${version}-linux-amd64.tar.gz`;
const archive = join(output, archiveName);
let createdOutput = false;
let completed = false;

const services = [
  {
    name: "sprite-desktop",
    cmd: "/opt/sprite-desktop/current/bin/desktop.sh",
    args: [],
    http_port: null,
    needs: [],
    env: {},
    dir: "/home/sprite",
  },
  {
    name: "sprite-desktop-bridge",
    cmd: "/opt/sprite-desktop/current/bin/sprite-desktop-bridge",
    args: ["--config", "/etc/sprite-desktop/config.json"],
    http_port: 8080,
    needs: [],
    env: {},
    dir: "/home/sprite",
  },
];
const packages = [
  "tigervnc-standalone-server",
  "tigervnc-common",
  "xfce4",
  "xfce4-terminal",
  "dbus-x11",
  "xubuntu-wallpapers",
  "fonts-dejavu-core",
  "xdg-utils",
  "curl",
  "ca-certificates",
  "firefox",
  "libglycin-2-0",
  "glycin-loaders",
  "glycin-thumbnailers",
];

try {
  await mkdir(join(root, "dist", "releases"), { recursive: true });
  await mkdir(output); // Never replace an existing versioned release.
  createdOutput = true;
  await mkdir(join(payload, "bin"), { recursive: true });
  await exec("pnpm", ["--filter", "@sprite-desktop/viewer", "build"], {
    cwd: root,
  });

  await exec(
    "go",
    [
      "build",
      "-trimpath",
      "-buildvcs=false",
      "-ldflags",
      `-s -w -X main.buildVersion=${version} -X main.buildSource=${source}`,
      "-o",
      join(payload, "bin", "sprite-desktop-bridge"),
      "./bridge",
    ],
    {
      cwd: root,
      env: { ...process.env, CGO_ENABLED: "0", GOOS: "linux", GOARCH: "amd64" },
    },
  );
  await copyFile(
    join(root, "installer", "desktop.sh"),
    join(payload, "bin", "desktop.sh"),
  );
  await Promise.all([
    chmod(join(payload, "bin", "sprite-desktop-bridge"), 0o555),
    chmod(join(payload, "bin", "desktop.sh"), 0o555),
  ]);

  const manifest = {
    schema: 1,
    release: version,
    source,
    os: { id: "ubuntu", codename: "resolute", architecture: "amd64" },
    versions: {
      novnc: "1.7.0",
      websocket: "github.com/gorilla/websocket@v1.5.3",
    },
    ports: { rfb_loopback: 5900, http: 8080 },
    sockets: {
      rfb: "/tmp/sprite-desktop/rfb.sock",
      sprite_api: "/.sprite/api.sock",
    },
    services,
  };
  const sources = {
    schema: 1,
    sources: [
      {
        name: "ubuntu",
        url: "https://archive.ubuntu.com/ubuntu",
        suites: [
          "resolute",
          "resolute-updates",
          "resolute-security",
          "resolute-proposed",
        ],
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
    glycin: {
      suite: "resolute-proposed",
      version: "2.1.5+ds-0ubuntu0.1",
      packages: ["libglycin-2-0", "glycin-loaders", "glycin-thumbnailers"],
    },
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

  const template = await readFile(
    join(root, "installer", "install.sh"),
    "utf8",
  );
  const archiveURL = new URL(
    `${version}/${archiveName}`,
    `${baseURL.replace(/\/*$/u, "")}/`,
  ).href;
  const installer = template
    .replaceAll("@SPRITE_DESKTOP_VERSION@", version)
    .replaceAll("@SPRITE_DESKTOP_ARCHIVE_URL@", archiveURL)
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
