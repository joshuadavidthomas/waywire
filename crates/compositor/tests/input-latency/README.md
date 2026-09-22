# Opt-in browser-dispatch-to-native-pixels latency

This measures one browser `performance.now()` clock: immediately before a
synchronous keydown dispatch into the production SDK, through the first
production `drawImage` containing the exact native response marker. It does not
measure hardware input or physical display scanout. Input-sequence acknowledgment
is recorded separately and is **not** accepted as a visual response. Network time
between sender and browser is included, never subtracted using unrelated clocks.

Production codec, quality adaptation, feedback, decoding and presentation are
unchanged. The harness uses the production `Playout` class with the viewer's
per-draw and one-second idle update cadence (initial target 100 ms, adaptive).
Wire metadata records actual generation/dimensions/FPS/chroma; quality events,
per-draw statistics, rAF intervals, timeouts, randomized phase delays and pixel
readback durations are saved. Initial FFmpeg argv and sender binary hashes are
also saved. Requested FPS is not assumed to be achieved FPS.

For a fixed-target comparison, append `?latencyTargetMs=100` to the harness URL
before running the same browser API. This sets the public SDK latency option and
disables only the harness's adaptive `Playout` updates. Production feedback and
quality adaptation remain enabled. The export and each block explicitly record
`playout: { mode: "fixed", targetMs: 100 }`; per-draw stats record the actual target.
Without the query parameter the original adaptive mode remains the default.
Use a fresh page and a separate `LATENCY_BUILD` directory for a new experiment,
so its results cannot overwrite an earlier suite. `?latencyTargetMs=300` is useful
for a bounded high-rate buffering correctness check, not a throughput claim.

### Playback-delay screening at nominal 60 FPS

`latency.runPlaybackSuite()` runs three interleaved repeats of ten responses for
each of fixed 0/25/50/100 ms and adaptive mode, quiet and motion: 300 responses,
12-second warmups, always requested 60 FPS. It records each block's target mode
and saves `playback-natural.json`, leaving the older `results.json` untouched.
For local correctness only, use
`runPlaybackSuite({ repeats: 1, count: 3, warmupMs: 2500, name: "local-smoke" })`.
`runBlock` also accepts a final `{ mode: "fixed", targetMs: 50 }` or
`{ mode: "adaptive" }` argument; omitted arguments preserve existing behavior.
`save("experiment-name")` writes a separate named result file.

On a fresh page, `runJitterSuite({ targetMs: 50 })` compares fixed100 against
fixed50 for motion at nominal60, three interleaved ten-response repeats. It adds
a 90ms sender-to-browser video relay pause every two seconds; control/pongs and
reverse traffic are untouched. TCP order is preserved. Actual pause durations
are recorded using a separate sender-relative clock, not subtracted from client
latency. This is an artificial head-of-line test, not a physical-network model.
Save name defaults to `playback-jitter`; the natural suite never enables pauses.
The same repeats/count/warmupMs options permit a bounded local correctness smoke.

Clock samples are logged by a study-only wrapper around the real
`ClockSynchronizer.update`, reading its existing acceptance timestamp. The wrapper
does not estimate a second clock or change sample acceptance. This diagnostic is
pinned to this source layout and includes accepted/rejected samples, best RTT,
offset and confidence; never use those offsets to infer real one-way latency.

The deterministic production-runtime replay runs without a browser/codec:

```sh
PLAYBACK_REPLAY_OUT=/tmp/playback-replay.json pnpm --filter @waywire/web exec \
  tsx --test "$PWD/crates/compositor/tests/input-latency/playback-replay.test.ts"
```

It compares immediate newest, fixed 0/25/50/100 and adaptive scheduling at 60 Hz.
Synthetic paths specify stable 30/30 ms, an RTT step to 70/70 ms, asymmetric
10/50 ms, and ordered-TCP downstream jitter `[0,0,15,50,0,90,5]` ms added to 30 ms.
Encoding is a fixed synthetic 8 ms and decode batches drain on 4 ms completion
ticks; these are scheduling tests, not measured encoder/decoder throughput.
Ground-truth capture age is available only in this artificial replay. Raw draws,
clock accept/reject decisions and confidence transitions are exported separately
from initial buffering and steady-state summaries.

### Real gateway clock-recovery regression

On a fresh page, run `latency.runClockRecovery()` and export with
`latency.export()`. It saves `clock-recovery.json`, including partial evidence
on failure. Use separate sender and browser machines/orbs: competing encoder and
decoder workloads can cause unrelated quality changes or decoder resets.

The opt-in relay delays real control pings and pongs by 30 ms each way, briefly
raises this to 70 ms each way, restores 30 ms, then sustains 70 ms. It preserves
IDs, the gateway's `serverNanos`, and message order. Video traffic is not delayed.
This exercises the production filter and presentation path, not a whole-network
simulation or a performance comparison. Network RTT is additional to these
injected delays. Ordinary conditions default to zero injected delay.

The test requires rejection of the transient sample without losing confidence,
then two fresh samples after confidence expires on the sustained slower path.
Reacquisition must finish within four seconds after a visible fallback response.
Exact native markers must reach the canvas before, during, and after the change;
capacity loss, decoder resets after initial synchronization, or queue-bound
violations fail the check. Live queue/confidence polls and actual relay delays
are retained alongside draw-linked statistics. Stop client and sender using the
same cleanup calls below. A timeout may indicate host/transport noise: inspect the
raw evidence rather than silently discarding it or claiming a latency benefit.

## Build and run

From the repository root, after `.agents/setup`:

```sh
uv run crates/compositor/tests/input-latency/server.py --prepare
amp orb service start input-latency \
  --command 'uv run crates/compositor/tests/input-latency/server.py' --portal
```

The service prints its authenticated portal URL. Open that URL in Chromium on a
**different machine/orb** from the sender. Build artifacts and live results live
under `/tmp/waywire-input-latency` (`LATENCY_BUILD` can override this directory).
No builds occur during measurements. Only one gateway/compositor/fixture runs;
changing conditions stops the old gateway before starting the next. The gateway
uses explicit 1920×1080, 16000 kbps, and the selected 60/90/120 FPS. CRF, encoder
and keyframe cadence remain the repository defaults, recorded in actual argv.

Browser console or `agent-browser eval`:

```js
await latency.correctness(); // delayed pixels vs early ack; stale/timeout recovery
// Start without blocking the agent-browser command for the whole suite:
window.latencyDone = null;
window.latencyError = null;
latency
  .runSuite()
  .then((value) => (window.latencyDone = value))
  .catch((error) => (window.latencyError = String(error)));
// Poll:
({
  done: window.latencyDone,
  error: window.latencyError,
  state: latency.state(),
});
// Export (also auto-saved on sender after each block):
JSON.stringify(latency.export());
// After capturing the representative canvas, stop client and sender:
await latency.stop();
await fetch("/api/stop", { method: "POST" });
```

The suite runs three interleaved blocks of 20 responses for each of six
conditions (quiet/motion × 60/90/120), with a 12-second warmup per block. Inputs
wait a fresh cryptographic-random 70–240 ms timer interval, not an rAF edge. One
extra 60-FPS motion block amplifies pixel readback fivefold as a sensitivity
check. Do not run other measurements on either machine concurrently. Record the
client CPU model/count and Chromium version/binary hash separately. Same-orb
smoke results must not be mixed into the separated experiment.

```sh
python3 crates/compositor/tests/input-latency/analyze.py \
  /tmp/waywire-input-latency/results.json > /tmp/latency-summary.json
```

The summary excludes correctness checks and retains per-block p50/p95, actual
settings, clock confidence, RTT, rAF/readback cost, drops and achieved receive,
decode and canvas-presentation rates. p95 uses nearest rank. Small-sample tail
estimates and restart/host variance must remain visible in interpretation.

## Fixture contract and correctness

`WAYWIRE_LATENCY=quiet|motion` is opt-in; normal receiver and `WAYWIRE_BENCH`
behavior are unchanged. F8 increments a 16-bit response marker immediately. F9
increments it but delays visible pixels by 350 ms while motion/acknowledgments
continue. A newer F8 supersedes a pending F9. Markers never wrap within a session.
After a timeout, the harness sends a fresh key and requires its new exact marker;
it stops rather than silently proceeding if resynchronization fails.

Magenta/cyan anchors precede 16 grayscale cells and a complementary second row.
Both rows are sampled independently after each draw. Four fixed-size shared
buffers are prepainted. Quiet backgrounds are identical across all buffers.
Motion alternates a smooth RGB gradient by eight brightness levels across every
pixel; it is a continuous full-frame **low-entropy** load, not a worst-case
encoder-throughput workload. Only marker rectangles are repainted in steady
state. A buffer is never modified or reattached until its Wayland release event.
The fixture intentionally rejects resizing during a run.

```sh
node --test crates/compositor/tests/input-latency/marker.test.js
cargo build --locked -p waywire-compositor --bins --examples
python3 crates/compositor/tests/input-latency/native-test.py
python3 crates/compositor/tests/native-scene.py
PATH="$HOME/.local/share/waywire-xwayland/bin:$PATH" \
  python3 crates/compositor/tests/interop.py
```

The native test observes real raw frames on both quiet and motion paths. It
checks that receiving F9 does not immediately change pixels, and that a later
F8 cannot be overwritten by a stale delayed response. It also checks every marker
through 24, including the 16→17 boundary, and byte-identical quiet backgrounds
across buffer rotation. The browser correctness
check additionally verifies that an early compositor ack precedes the matching
canvas draw by at least 100 ms. Pixel readback may synchronize GPU work and
affect later frames; its cost and amplified-read sensitivity must be reported.
