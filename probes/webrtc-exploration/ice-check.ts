import { execFile } from "node:child_process";
import { promisify } from "node:util";
import { writeFile } from "node:fs/promises";
const exec = promisify(execFile);
const script = `(() => {
  const Native = window.RTCPeerConnection;
  const log = window.iceProbe = { states: [], candidates: [], samples: [] };
  window.RTCPeerConnection = class extends Native {
    constructor(config) {
      super(config);
      this.addEventListener('iceconnectionstatechange', () => log.states.push({ice: this.iceConnectionState, peer: this.connectionState}));
      this.addEventListener('connectionstatechange', () => log.states.push({ice: this.iceConnectionState, peer: this.connectionState}));
      this.addEventListener('icecandidate', e => log.candidates.push({type: e.candidate?.type ?? 'complete', protocol: e.candidate?.protocol}));
      const timer = setInterval(async () => {
        if (log.samples.length >= 25 || this.connectionState === 'closed') { clearInterval(timer); return; }
        const stats = await this.getStats();
        const sample = [];
        stats.forEach(s => {
          if (s.type === 'candidate-pair') sample.push({type:s.type, state:s.state, nominated:s.nominated, requestsSent:s.requestsSent, responsesReceived:s.responsesReceived, requestsReceived:s.requestsReceived, responsesSent:s.responsesSent});
          if (s.type === 'transport') sample.push({type:s.type, dtlsState:s.dtlsState, iceState:s.iceState});
          if (s.type === 'remote-candidate' || s.type === 'local-candidate') sample.push({type:s.type,candidateType:s.candidateType,protocol:s.protocol});
        });
        log.samples.push(sample);
      },1000);
    }
  };
  return 'installed';
})()`;
const args = ["--session", "rust-webrtc-sprite", "--json"];
await exec("agent-browser", [
  ...args,
  "eval",
  "-b",
  Buffer.from(script).toString("base64"),
]);
await exec("agent-browser", [...args, "click", "#connect"]);
await new Promise((resolve) => setTimeout(resolve, 22_000));
const result = await exec("agent-browser", [
  ...args,
  "eval",
  "({ice:window.iceProbe, connected:window.webrtcProbe.connected, status:document.querySelector('#status').textContent, control:document.querySelector('#control-status').textContent})",
]);
const path = `probes/webrtc-exploration/results/sprite-ice-${new Date().toISOString().replaceAll(":", "-")}.json`;
await writeFile(path, result.stdout);
console.log(path);
console.log(result.stdout);
