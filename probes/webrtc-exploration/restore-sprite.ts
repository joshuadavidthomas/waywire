import { writeFile } from "node:fs/promises";
import { loadTrialSprite, trialServiceName } from "../rust-desktop/trial.js";
import { restartTrial } from "../rust-desktop/restart-trial.js";
const sprite = await loadTrialSprite("sprite-desktop-rust");
const destination =
  "probes/webrtc-exploration/results/sprite-network-2026-09-07";
const events: unknown[] = [];
try {
  const logs = await sprite.getServiceLogs(trialServiceName, {
    lines: 80,
    duration: "1s",
  });
  await logs.processAll((event) => {
    events.push(event);
  });
} catch (error) {
  events.push({ logReadFailed: String(error) });
}
await writeFile(
  `${destination}/baseline-service-before-restart.json`,
  JSON.stringify(events, null, 2),
);
console.log(JSON.stringify(events, null, 2));
const restored = await restartTrial(sprite);
const report = {
  previousHealth: "connection refused after restoring original output size",
  binariesUploaded: false,
  operation: "recreate identical owned service definition",
  restored,
};
await writeFile(
  `${destination}/baseline-restored.json`,
  JSON.stringify(report, null, 2),
);
console.log(JSON.stringify(report, null, 2));
