import assert from "node:assert/strict";
import { randomBytes } from "node:crypto";
import { readFile, writeFile } from "node:fs/promises";
import { parseArgs } from "node:util";
import { SpritesClient } from "@fly/sprites";

const { values } = parseArgs({
  options: { release: { type: "string" } },
  strict: true,
});
assert(
  values.release &&
    /^v\d+\.\d+\.\d+(?:[-+][0-9A-Za-z.-]+)?$/u.test(values.release),
);
assert(process.env.SPRITES_TOKEN);
const sprite = await new SpritesClient(process.env.SPRITES_TOKEN).getSprite(
  "sprite-desktop-v1-conflicts",
);
const fs = sprite.filesystem("/");
const remote = `/tmp/desktop-preflight-${randomBytes(8).toString("hex")}`;
await fs.mkdir(remote);
await fs.writeFile(
  `${remote}/install.sh`,
  await readFile(
    new URL(`../dist/releases/${values.release}/install.sh`, import.meta.url),
  ),
  { mode: 0o700 },
);
await fs.writeFile(
  `${remote}/archive`,
  await readFile(
    new URL(
      `../dist/releases/${values.release}/sprite-desktop-${values.release}-linux-amd64.tar.gz`,
      import.meta.url,
    ),
  ),
);
const results: unknown[] = [];
async function refused(message: RegExp, env: string[] = []) {
  const services = await sprite.listServices();
  const command = sprite.spawn("env", [
    ...env,
    "bash",
    `${remote}/install.sh`,
    "--archive",
    `${remote}/archive`,
  ]);
  let stdout = "",
    stderr = "";
  command.stdout.on("data", (chunk) => {
    stdout += chunk;
  });
  command.stderr.on("data", (chunk) => {
    stderr += chunk;
  });
  const code = await command.wait();
  assert.notEqual(code, 0);
  assert.match(stderr, message);
  assert(
    !stdout.includes("configuring verified apt"),
    "refusal occurred after apt mutation",
  );
  assert.deepEqual(
    await sprite.listServices(),
    services,
    "refusal changed a foreign service",
  );
  await sprite.execFile("test", ["!", "-e", "/etc/sprite-desktop"]);
  await sprite.execFile("test", ["!", "-e", "/var/lib/sprite-desktop"]);
  results.push({ reason: message.source, passed: true });
}
try {
  await sprite.execFile("sudo", [
    "cp",
    "/etc/os-release",
    `${remote}/os-release.saved`,
  ]);
  await fs.writeFile(
    `${remote}/unsupported-os`,
    "ID=debian\nVERSION_CODENAME=trixie\n",
  );
  try {
    await sprite.execFile("sudo", [
      "cp",
      `${remote}/unsupported-os`,
      "/etc/os-release",
    ]);
    await refused(/only Ubuntu 26.04/u);
  } finally {
    await sprite.execFile("sudo", [
      "cp",
      `${remote}/os-release.saved`,
      "/etc/os-release",
    ]);
  }

  await fs.writeFile(
    `${remote}/dpkg`,
    '#!/usr/bin/env bash\nif [ "$1" = --print-architecture ]; then printf "arm64\\n"; else exec /usr/bin/dpkg "$@"; fi\n',
    { mode: 0o700 },
  );
  const path = (await sprite.execFile("printenv", ["PATH"])).stdout
    .toString()
    .trim();
  await refused(/only amd64/u, [`PATH=${remote}:${path}`]);

  for (const name of [
    "sprite-desktop",
    "sprite-desktop-bridge",
    "foreign-http",
  ]) {
    (
      await sprite.createService(
        name,
        {
          cmd: "sleep",
          args: ["600"],
          ...(name === "foreign-http" ? { httpPort: 9090 } : {}),
        },
        "1s",
      )
    ).close();
    try {
      await refused(
        name === "foreign-http"
          ? /foreign HTTP service/u
          : /service name is foreign/u,
      );
    } finally {
      await sprite.deleteService(name);
    }
  }
  for (const port of [5900, 8080]) {
    (
      await sprite.createService(
        "foreign-listener",
        {
          cmd: "python3",
          args: ["-m", "http.server", String(port), "--bind", "127.0.0.1"],
        },
        "1s",
      )
    ).close();
    try {
      await refused(
        new RegExp(`port ${port} is occupied by an unmanaged process`, "u"),
      );
    } finally {
      await sprite.deleteService("foreign-listener");
    }
  }
  const policy = await sprite.getNetworkPolicy();
  try {
    await sprite.updateNetworkPolicy({
      rules: [
        { domain: "archive.ubuntu.com", action: "allow" },
        { domain: "packages.mozilla.org", action: "deny" },
      ],
    });
    await refused(/apt source is unreachable: https:\/\/packages.mozilla.org/u);
  } finally {
    await sprite.updateNetworkPolicy(policy);
  }
  await writeFile(
    new URL("results/runtime-preflight.json", import.meta.url),
    JSON.stringify(
      {
        sprite: sprite.name,
        release: values.release,
        at: new Date().toISOString(),
        results,
      },
      null,
      2,
    ) + "\n",
  );
  console.log(JSON.stringify(results, null, 2));
} finally {
  await fs.rm(remote, { recursive: true, force: true });
}
