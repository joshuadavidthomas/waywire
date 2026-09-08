import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { execFile } from "node:child_process";
import { mkdir, readFile, writeFile } from "node:fs/promises";
import { promisify } from "node:util";
import { setTimeout as sleep } from "node:timers/promises";
import { LocalCdp } from "./cdp.js";

const execute = promisify(execFile);
function argument(index: number): string {
  const value = process.argv[index];
  assert(value, `missing argument ${index - 1}`);
  return value;
}
const endpoint = argument(2);
const origin = argument(3);
const container = argument(4);
const output = argument(5);
const browserCgroup = argument(6);
const containerCgroup = argument(7);
const rateText = argument(8);
const bitrate = Number(argument(9));
assert(bitrate === 8000 || bitrate === 16000);
assert.match(origin, /^http:\/\/127\.0\.0\.1:\d+$/);
assert.match(container, /^socket-recheck-[a-z0-9-]+$/);
const rate = Number(rateText);
assert(rate === 30 || rate === 60);
await mkdir(output, { recursive: true });

const instrumentation = `(() => {
  const s = globalThis.__socketRecheck = {submitted:0, decoded:0, generation:0, latestGeneration:0, drawCallbacks:0, putCallbacks:0, outputs:[], errors:[]};
  addEventListener('error', e => s.errors.push(String(e.message).slice(0,300)));
  addEventListener('unhandledrejection', e => s.errors.push(String(e.reason).slice(0,300)));
  if (globalThis.VideoDecoder) {
    const Native=globalThis.VideoDecoder;
    globalThis.VideoDecoder=new Proxy(Native,{construct(Target,args){
      const options=args[0]; let generation=0;
      return new Target({...options,output(frame){generation=s.generation;s.decoded++;s.latestGeneration=generation;s.outputs.push({generation,width:frame.displayWidth,height:frame.displayHeight,format:frame.format,colorSpace:{fullRange:frame.colorSpace.fullRange,matrix:frame.colorSpace.matrix,primaries:frame.colorSpace.primaries,transfer:frame.colorSpace.transfer}});if(s.outputs.length>40)s.outputs.shift();options.output(frame);}});
    }});
    const decode=Native.prototype.decode;
    Native.prototype.decode=function(chunk){s.submitted++;return decode.call(this,chunk);};
    const configure=Native.prototype.configure;
    Native.prototype.configure=function(config){s.generation++; this.__socketGeneration=s.generation; const result=configure.call(this,config); return result;};
  }
  const draw=CanvasRenderingContext2D.prototype.drawImage;
  CanvasRenderingContext2D.prototype.drawImage=function(...args){if(this.canvas?.id==='display')s.drawCallbacks++;return draw.apply(this,args);};
  const put=CanvasRenderingContext2D.prototype.putImageData;
  CanvasRenderingContext2D.prototype.putImageData=function(...args){if(this.canvas?.id==='display')s.putCallbacks++;return put.apply(this,args);};
})();`;

function sha(bytes: Buffer): string {
  return createHash("sha256").update(bytes).digest("hex");
}
async function cpu(path: string): Promise<number> {
  const text = await readFile(`${path}/cpu.stat`, "ascii");
  const match = text.match(/^usage_usec (\d+)$/m);
  assert(match, `missing usage_usec in ${path}`);
  return Number(match[1]);
}
async function cpuWindow(kind: "quiet" | "motion") {
  await execute(
    "docker",
    [
      "exec",
      container,
      "sh",
      "-c",
      `printf ${kind} > /tmp/socket-local/fixture-mode`,
    ],
    { timeout: 5000 },
  );
  await sleep(2000);
  const picturesBefore = await stats();
  const before = {
    browser: await cpu(browserCgroup),
    container: await cpu(containerCgroup),
  };
  const started = performance.now();
  await sleep(45000);
  const after = {
    browser: await cpu(browserCgroup),
    container: await cpu(containerCgroup),
  };
  const seconds = (performance.now() - started) / 1000;
  const picturesAfter = await stats();
  assert(
    after.browser >= before.browser && after.container >= before.container,
    "CPU counters regressed",
  );
  return {
    kind,
    seconds,
    before,
    after,
    decodedFrames: picturesAfter.decoded - picturesBefore.decoded,
    drawnFrames: picturesAfter.drawCallbacks - picturesBefore.drawCallbacks,
    browserCpuSeconds: (after.browser - before.browser) / 1e6,
    containerCpuSeconds: (after.container - before.container) / 1e6,
    browserOneCorePercent: (after.browser - before.browser) / 1e4 / seconds,
    containerOneCorePercent:
      (after.container - before.container) / 1e4 / seconds,
  };
}
async function source(name: string): Promise<{ hash: string; bytes: Buffer }> {
  await execute(
    "docker",
    [
      "exec",
      "-e",
      "XDG_RUNTIME_DIR=/tmp/socket-local",
      "-e",
      "WAYLAND_DISPLAY=wayland-0",
      container,
      "grim",
      `/evidence/${name}.png`,
    ],
    { timeout: 10000 },
  );
  const bytes = await readFile(`${output}/${name}.png`);
  return { hash: sha(bytes), bytes };
}
async function psnr(
  sourceName: string,
  decodedName: string,
  width: number,
  height: number,
) {
  const crops = [
    { name: "whole", filter: "null" },
    { name: "top-left", filter: `crop=${width / 2}:${height / 2}:0:0` },
    {
      name: "top-right",
      filter: `crop=${width / 2}:${height / 2}:${width / 2}:0`,
    },
    {
      name: "bottom-left",
      filter: `crop=${width / 2}:${height / 2}:0:${height / 2}`,
    },
    {
      name: "bottom-right",
      filter: `crop=${width / 2}:${height / 2}:${width / 2}:${height / 2}`,
    },
  ];
  const rows = [];
  for (const crop of crops) {
    const graph = `[0:v]format=gbrp,${crop.filter}[a];[1:v]format=gbrp,${crop.filter}[b];[a][b]psnr`;
    const result = await execute(
      "ffmpeg",
      [
        "-hide_banner",
        "-i",
        `${output}/${sourceName}.png`,
        "-i",
        `${output}/${decodedName}.png`,
        "-filter_complex",
        graph,
        "-f",
        "null",
        "-",
      ],
      { timeout: 20000 },
    );
    const match = result.stderr.match(/average:([0-9.]+|inf)/);
    assert(match, `PSNR missing for ${crop.name}`);
    rows.push({
      crop: crop.name,
      rgbAverageDb: match[1] === "inf" ? null : Number(match[1]),
      identical: match[1] === "inf",
    });
  }
  return rows;
}

const cdp = await LocalCdp.connect(endpoint);
let session = "";
let targetId = "";
const log: string[] = [];
cdp.onEvent = (method, params) => {
  if (method === "Runtime.consoleAPICalled") log.push(`console:${params.type}`);
  if (method === "Runtime.exceptionThrown")
    log.push(
      `exception:${String(params.exceptionDetails?.text).slice(0, 200)}`,
    );
};
async function evaluate(expression: string): Promise<any> {
  const result = await cdp.call(
    "Runtime.evaluate",
    { expression, awaitPromise: true, returnByValue: true },
    session,
  );
  assert(
    !result.exceptionDetails,
    result.exceptionDetails?.text ?? "browser evaluation failed",
  );
  return result.result.value;
}
async function stats() {
  return evaluate(
    `(()=>{const s=globalThis.__socketRecheck,c=document.querySelector('#display');return {...s,outputs:s?.outputs?.slice(-8),canvas:c?{width:c.width,height:c.height,css:(()=>{const r=c.getBoundingClientRect();return {x:r.x,y:r.y,width:r.width,height:r.height}})()}:null,status:document.querySelector('#status')?.textContent,control:document.querySelector('#control-status')?.textContent,hidden:document.hidden};})()`,
  );
}
async function waitFor(
  predicate: (value: any) => boolean,
  message: string,
  attempts = 60,
) {
  for (let index = 0; index < attempts; index++) {
    const value = await stats();
    if (predicate(value)) return value;
    await sleep(500);
  }
  throw new Error(message);
}
async function setSize(width: number, height: number) {
  await cdp.call(
    "Emulation.setDeviceMetricsOverride",
    {
      width: width + 40,
      height: height + 180,
      deviceScaleFactor: 1,
      mobile: false,
    },
    session,
  );
  await evaluate(
    `(()=>{const shell=document.querySelector('.display-shell');Object.assign(shell.style,{boxSizing:'content-box',width:'${width}px',height:'${height}px',minWidth:'${width}px',minHeight:'${height}px',maxWidth:'${width}px',maxHeight:'${height}px'});})()`,
  );
  return waitFor(
    (s) =>
      s.canvas?.width === width &&
      s.canvas?.height === height &&
      s.outputs.at(-1)?.width === width &&
      s.outputs.at(-1)?.height === height,
    `resize ${width}x${height} did not cross the browser control channel`,
  );
}

try {
  const targets = await cdp.call("Target.getTargets");
  const blank = targets.targetInfos.find(
    (target: any) => target.type === "page" && target.url === "about:blank",
  );
  assert(blank, "owned blank Chrome target missing");
  targetId = blank.targetId;
  ({ sessionId: session } = await cdp.call("Target.attachToTarget", {
    targetId,
    flatten: true,
  }));
  await cdp.call("Page.enable", {}, session);
  await cdp.call("Runtime.enable", {}, session);
  await cdp.call("Network.enable", {}, session);
  await cdp.call(
    "Page.addScriptToEvaluateOnNewDocument",
    { source: instrumentation },
    session,
  );
  await cdp.call(
    "Emulation.setDeviceMetricsOverride",
    { width: 1320, height: 900, deviceScaleFactor: 1, mobile: false },
    session,
  );
  await sleep(20000);
  await cdp.call("Page.navigate", { url: origin }, session);
  await waitFor((s) => Boolean(s.canvas), "viewer canvas did not load", 90);
  const attached = await setSize(1280, 720);
  assert(attached.decoded > 0, "no decoded 1280 frame");
  assert.equal(
    await evaluate(
      "getComputedStyle(document.querySelector('#display')).objectFit",
    ),
    "contain",
  );
  assert.equal(attached.outputs.at(-1).format, "I444");
  assert.equal(attached.outputs.at(-1).colorSpace.fullRange, true);
  await cdp.call("Network.disable", {}, session);

  const quietCpu = await cpuWindow("quiet");
  await writeFile(
    `${output}/quiet-cpu.json`,
    JSON.stringify(quietCpu, null, 2),
  );
  await sleep(3000);
  const before = await source("source-before");
  const decoded = Buffer.from(
    await evaluate(
      `document.querySelector('#display').toDataURL('image/png').split(',')[1]`,
    ),
    "base64",
  );
  await writeFile(`${output}/decoded-1280.png`, decoded);
  const after = await source("source-after");
  assert.equal(
    before.hash,
    after.hash,
    "source changed across decoded canvas readback",
  );

  const motionCpu = await cpuWindow("motion");
  await writeFile(
    `${output}/motion-cpu.json`,
    JSON.stringify(motionCpu, null, 2),
  );
  const beforeBackground = await stats();
  const extra = await cdp.call("Target.createTarget", {
    url: "about:blank",
    background: false,
  });
  await cdp.call("Target.activateTarget", { targetId: extra.targetId });
  const hidden = await waitFor(
    (s) => s.hidden === true,
    "desktop tab never became hidden",
  );
  await sleep(35000);
  await cdp.call("Target.activateTarget", { targetId });
  await cdp.call("Page.bringToFront", {}, session);
  await cdp.call(
    "Input.dispatchMouseEvent",
    { type: "mouseMoved", x: 300, y: 300 },
    session,
  );
  const afterBackground = await waitFor(
    (s) => !s.hidden && s.decoded > beforeBackground.decoded,
    "decode did not resume after 35 second hidden interval",
  );
  await cdp.call("Target.closeTarget", { targetId: extra.targetId });

  const canvas = afterBackground.canvas;
  await cdp.call(
    "Input.dispatchMouseEvent",
    {
      type: "mousePressed",
      x: canvas.css.x + canvas.css.width / 2,
      y: canvas.css.y + canvas.css.height / 2,
      button: "left",
      clickCount: 1,
    },
    session,
  );
  await cdp.call(
    "Input.dispatchMouseEvent",
    {
      type: "mouseReleased",
      x: canvas.css.x + canvas.css.width / 2,
      y: canvas.css.y + canvas.css.height / 2,
      button: "left",
      clickCount: 1,
    },
    session,
  );
  await waitFor(
    (s) => /Input active/.test(s.control ?? ""),
    "input control was not acquired",
  );
  await cdp.call(
    "Input.dispatchKeyEvent",
    {
      type: "keyDown",
      key: "Shift",
      code: "ShiftLeft",
      windowsVirtualKeyCode: 16,
      modifiers: 8,
    },
    session,
  );
  await execute("docker", ["network", "disconnect", "bridge", container], {
    timeout: 10000,
  });
  await waitFor(
    (s) => /reconnecting|disconnected/i.test(s.control ?? ""),
    "control did not disconnect during network loss",
  );
  // Release the local modifier while offline; no new controller can send a
  // compensating release to hide a stuck key in the old native lease.
  await cdp.call(
    "Input.dispatchKeyEvent",
    {
      type: "keyUp",
      key: "Shift",
      code: "ShiftLeft",
      windowsVirtualKeyCode: 16,
    },
    session,
  );
  await execute("docker", ["network", "connect", "bridge", container], {
    timeout: 10000,
  });
  const reconnected = await waitFor(
    (s) =>
      /Connected/.test(s.status ?? "") && /Input active/.test(s.control ?? ""),
    "channels did not reconnect",
    120,
  );
  await cdp.call(
    "Input.dispatchKeyEvent",
    {
      type: "keyDown",
      key: "a",
      code: "KeyA",
      windowsVirtualKeyCode: 65,
      text: "a",
    },
    session,
  );
  await cdp.call(
    "Input.dispatchKeyEvent",
    { type: "keyUp", key: "a", code: "KeyA", windowsVirtualKeyCode: 65 },
    session,
  );
  let fixture: any;
  for (let i = 0; i < 40; i++) {
    fixture = JSON.parse(
      (
        await execute(
          "docker",
          ["exec", container, "cat", "/home/sprite/fixture-state.json"],
          { timeout: 5000 },
        )
      ).stdout,
    );
    if (fixture.keys.endsWith("a")) break;
    await sleep(250);
  }
  assert(fixture.keys.endsWith("a"), "post-reconnect key was not lowercase");

  const generation1280 = (await stats()).outputs.at(-1).generation;
  const at1920 = await setSize(1920, 1080);
  assert(
    at1920.outputs.at(-1).generation > generation1280,
    "1920 frame was not produced by a new decoder generation",
  );
  await execute(
    "docker",
    [
      "exec",
      container,
      "sh",
      "-c",
      "printf quiet > /tmp/socket-local/fixture-mode",
    ],
    { timeout: 5000 },
  );
  await sleep(3000);
  const source1920a = await source("source-1920-before");
  const decoded1920 = Buffer.from(
    await evaluate(
      `document.querySelector('#display').toDataURL('image/png').split(',')[1]`,
    ),
    "base64",
  );
  await writeFile(`${output}/decoded-1920.png`, decoded1920);
  const source1920b = await source("source-1920-after");
  assert.equal(
    source1920a.hash,
    source1920b.hash,
    "1920 source changed across readback",
  );
  const finalStats = await stats();
  assert(
    finalStats.decoded > 0 &&
      finalStats.drawCallbacks + finalStats.putCallbacks > 0,
    "decoded outputs lacked canvas presentation callbacks",
  );
  assert(
    !log.some((line) => /correlation stalled|metadata\/RTP/i.test(line)),
    "late correlation error reached browser",
  );
  const quality = {
    "1280x720": await psnr("source-before", "decoded-1280", 1280, 720),
    "1920x1080": await psnr("source-1920-before", "decoded-1920", 1920, 1080),
  };
  const result = {
    rate,
    bitrate,
    attached,
    quietCpu,
    motionCpu,
    quality,
    source1280Hash: before.hash,
    decoded1280Hash: sha(decoded),
    source1920Hash: source1920a.hash,
    decoded1920Hash: sha(decoded1920),
    beforeBackground: { decoded: beforeBackground.decoded },
    hidden: {
      hidden: hidden.hidden,
      decoded: hidden.decoded,
      durationSeconds: 35,
    },
    afterBackground: { decoded: afterBackground.decoded },
    reconnected: {
      decoded: reconnected.decoded,
      status: reconnected.status,
      control: reconnected.control,
    },
    fixture,
    finalStats,
    log,
  };
  await writeFile(
    `${output}/observations.json`,
    JSON.stringify(result, null, 2),
  );
  console.log(
    JSON.stringify({
      rate,
      decoded: finalStats.decoded,
      drawCallbacks: finalStats.drawCallbacks,
      putCallbacks: finalStats.putCallbacks,
      sourceStable: true,
      newGeneration: true,
      key: fixture.keys.slice(-1),
    }),
  );
} catch (error) {
  await writeFile(
    `${output}/failure.json`,
    JSON.stringify(
      { message: String(error), stats: await stats().catch(() => null), log },
      null,
      2,
    ),
  );
  throw error;
} finally {
  cdp.close();
}
