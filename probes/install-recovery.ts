import assert from "node:assert/strict";
import { randomBytes } from "node:crypto";
import { readFile, writeFile } from "node:fs/promises";
import { resolve } from "node:path";
import { parseArgs } from "node:util";
import { SpritesClient } from "@fly/sprites";

// Require an explicit disposable test target; never use the user's desktop.
const { values } = parseArgs({
  options: { release: { type: "string" }, sprite: { type: "string" } },
  strict: true,
});
const name = values.sprite;
assert(
  name && name.startsWith("sprite-desktop-v1-"),
  "--sprite must name a disposable sprite-desktop-v1-* target",
);
const release = values.release;
assert(release && /^v\d+\.\d+\.\d+(?:[-+][0-9A-Za-z.-]+)?$/u.test(release));
assert(process.env.SPRITES_TOKEN);
const client = new SpritesClient(process.env.SPRITES_TOKEN);
const sprite = await client.getSprite(name);
assert.equal(sprite.urlSettings?.auth, "sprite");
assert.equal(sprite.urlSettings.privateAccess, "admins");
const fs = sprite.filesystem("/");
const bundle = resolve(import.meta.dirname, "../dist/releases", release);
const archiveName = `sprite-desktop-${release}-linux-amd64.tar.gz`;
const remote = `/tmp/desktop-recovery-${randomBytes(8).toString("hex")}`;
await fs.mkdir(remote);
await fs.writeFile(
  `${remote}/install.sh`,
  await readFile(resolve(bundle, "install.sh")),
  { mode: 0o700 },
);
await fs.writeFile(
  `${remote}/archive`,
  await readFile(resolve(bundle, archiveName)),
  { mode: 0o600 },
);
const results: unknown[] = [];
const recordPath = "/var/lib/sprite-desktop/install.json";
const homeSentinel = "/home/sprite/v1-acceptance-persistence.txt";
const preferences = "/home/sprite/.config/xfce4/helpers.rc";
const preferenceText = "WebBrowser=firefox\nTerminalEmulator=xfce4-terminal\n";
await fs.writeFile(
  homeSentinel,
  "keep this file across installation and restart\n",
);
await fs.writeFile(preferences, preferenceText);

async function runInstaller(
  env: Record<string, string> = {},
  archive = `${remote}/archive`,
) {
  const command = sprite.spawn("env", [
    ...Object.entries(env).map(([key, value]) => `${key}=${value}`),
    "bash",
    `${remote}/install.sh`,
    "--archive",
    archive,
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
  return { code, stdout, stderr };
}
async function servicePIDs() {
  const services = (await sprite.listServices()).filter((service) =>
    ["sprite-desktop", "sprite-desktop-bridge"].includes(service.name),
  );
  return Object.fromEntries(
    services.map((service) => [service.name, service.state?.pid]),
  );
}
async function verify() {
  const record = JSON.parse(await fs.readFile(recordPath, "utf8"));
  assert.equal(record.state, "committed");
  assert.equal(record.release, release);
  const version = JSON.parse(
    await fs.readFile("/etc/sprite-desktop/version.json", "utf8"),
  );
  assert.equal(version.release, release);
  assert(
    Date.parse(version.observed_at) > 0,
    "package observations have no valid timestamp",
  );
  assert.equal(
    await fs.readFile(homeSentinel, "utf8"),
    "keep this file across installation and restart\n",
  );
  assert.equal(await fs.readFile(preferences, "utf8"), preferenceText);
  const health = await sprite.execFile("curl", [
    "-fsS",
    "http://127.0.0.1:8080/healthz",
  ]);
  const status = JSON.parse(health.stdout.toString());
  assert.equal(status.release, release);
  assert.equal(status.rfb, "listening");
}

try {
  // A damaged upload fails before changing the install record or running services.
  await fs.writeFile(`${remote}/bad-archive`, "not an archive");
  const beforeRecord = await fs.readFile(recordPath, "utf8");
  const beforePIDs = await servicePIDs();
  const rejected = await runInstaller({}, `${remote}/bad-archive`);
  assert.notEqual(rejected.code, 0);
  assert.match(rejected.stderr, /archive size mismatch/u);
  assert.equal(await fs.readFile(recordPath, "utf8"), beforeRecord);
  assert.deepEqual(await servicePIDs(), beforePIDs);
  results.push({
    test: "corrupt archive changes no runtime state",
    passed: true,
  });

  for (const point of [
    "after-pending",
    "after-service-1",
    "after-service-2",
    "before-commit",
  ]) {
    if (point !== "after-pending") {
      // Force real service creation before each interruption, rather than only
      // testing a no-op visit to the failpoint in an already healthy install.
      await sprite.deleteService("sprite-desktop-bridge");
      await sprite.deleteService("sprite-desktop");
    }
    const failed = await runInstaller({ SPRITE_DESKTOP_FAILPOINT: point });
    assert.notEqual(failed.code, 0, `${point} did not interrupt installation`);
    assert.match(failed.stderr, new RegExp(`test failpoint: ${point}`, "u"));
    const pending = JSON.parse(await fs.readFile(recordPath, "utf8"));
    assert.equal(pending.state, "pending");
    assert.equal(pending.release, release);
    const interruptedPIDs = await servicePIDs();
    const repaired = await runInstaller();
    assert.equal(repaired.code, 0, repaired.stdout + repaired.stderr);
    await verify();
    const repairedPIDs = await servicePIDs();
    for (const [service, pid] of Object.entries(repairedPIDs)) {
      assert(
        pid && pid !== interruptedPIDs[service],
        `${service} was not restarted during pending recovery`,
      );
    }
    results.push({ test: point, interruptedPIDs, repairedPIDs, passed: true });
    console.log(`Passed recovery from ${point}`);
  }

  const stablePIDs = await servicePIDs();
  const stableVersion = await fs.readFile(
    "/etc/sprite-desktop/version.json",
    "utf8",
  );
  const metadataPaths = [
    "/etc/sprite-desktop/config.json",
    "/etc/sprite-desktop/version.json",
  ];
  const stableFiles = (
    await sprite.execFile("stat", ["-c", "%i:%Y", ...metadataPaths])
  ).stdout.toString();
  const managedFile = `/opt/sprite-desktop/releases/${release}/bin/desktop.sh`;
  await sprite.execFile("sudo", ["chmod", "0777", managedFile]);
  await sprite.execFile("sudo", ["chown", "sprite:sprite", managedFile]);
  const rerun = await runInstaller();
  assert.equal(rerun.code, 0, rerun.stdout + rerun.stderr);
  await verify();
  assert.deepEqual(await servicePIDs(), stablePIDs);
  assert.equal(
    await fs.readFile("/etc/sprite-desktop/version.json", "utf8"),
    stableVersion,
  );
  assert.equal(
    (
      await sprite.execFile("stat", ["-c", "%i:%Y", ...metadataPaths])
    ).stdout.toString(),
    stableFiles,
  );
  assert.equal(
    (await sprite.execFile("stat", ["-c", "%U:%G:%a", managedFile])).stdout
      .toString()
      .trim(),
    "root:root:555",
  );
  results.push({
    test: "rerun repairs managed file owner/mode without rewriting unchanged metadata",
    passed: true,
  });
  results.push({
    test: "same-version rerun keeps both service PIDs and metadata",
    passed: true,
  });

  const configRun = await runInstaller({
    DESKTOP_ORIGIN: "https://acceptance.example.test",
  });
  assert.equal(configRun.code, 0, configRun.stdout + configRun.stderr);
  const changedPIDs = await servicePIDs();
  assert.equal(changedPIDs["sprite-desktop"], stablePIDs["sprite-desktop"]);
  assert.notEqual(
    changedPIDs["sprite-desktop-bridge"],
    stablePIDs["sprite-desktop-bridge"],
  );
  const restored = await runInstaller();
  assert.equal(restored.code, 0, restored.stdout + restored.stderr);
  await verify();
  results.push({
    test: "origin change restarts only bridge; canonical origin restored",
    passed: true,
  });

  for (const observedAt of [null, "2026-02-30T00:00:00Z"]) {
    const beforeRepair = await servicePIDs();
    const damaged = JSON.parse(
      await fs.readFile("/etc/sprite-desktop/version.json", "utf8"),
    );
    damaged.observed_at = observedAt;
    await fs.writeFile(
      `${remote}/damaged-version.json`,
      JSON.stringify(damaged),
    );
    await sprite.execFile("sudo", [
      "install",
      "-m",
      "0644",
      `${remote}/damaged-version.json`,
      "/etc/sprite-desktop/version.json",
    ]);
    const repair = await runInstaller();
    assert.equal(repair.code, 0, repair.stdout + repair.stderr);
    await verify();
    const afterRepair = await servicePIDs();
    assert.equal(afterRepair["sprite-desktop"], beforeRepair["sprite-desktop"]);
    assert.notEqual(
      afterRepair["sprite-desktop-bridge"],
      beforeRepair["sprite-desktop-bridge"],
    );
  }
  results.push({
    test: "missing and invalid observation dates are repaired and bridge restarts",
    passed: true,
  });

  await writeFile(
    resolve(import.meta.dirname, "results/runtime-installer.json"),
    JSON.stringify(
      { sprite: name, release, at: new Date().toISOString(), results },
      null,
      2,
    ) + "\n",
  );
  console.log(JSON.stringify(results, null, 2));
} finally {
  await fs.rm(remote, { recursive: true, force: true });
}
