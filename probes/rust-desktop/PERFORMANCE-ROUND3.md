# Persistent capture trial and delivery stalls

[Round four](./PERFORMANCE-ROUND4.md) adds proxy, browser-host CPU and TCP evidence
on the saved WLR binaries. It reproduces failures without the ext candidate.

On 2026-09-06, persistent main-output capture increased capture throughput in the private Rust trial. It passed one matched run, then failed repeat acceptance. The trial now runs the saved WLR capture baseline again. The worktree still builds the experimental ext capture implementation; it does not match the deployed daemon.

VNC, Waymote, their recordings and the release installer remain unchanged. Shared SHM leases, an encoder child using libav, browser workers and transport changes have not been implemented.

## What changed

Native tracing now has an inactive fast path: it avoids reading the clock, building records or taking the trace mutex. Recording epochs prevent a producer sampled before stop/start from publishing into the next recording. The bounded v2 trace adds actual capture-copy authorization, successful flush and pending-frame replacement observations.

Gateway tracing records RTP access-unit completion, metadata matching, publication to viewers and each viewer's socket-write start/completion. Its private file is exclusive and mode 0600; memory holds at most 65,536 records. A joined blocking worker serializes dumps. The probe validates process and path ownership before controlling either trace and rejects stale dumps.

The browser observer records bounded header, decoder and long-task events. It joins decoded output to actual canvas draws by WebCodecs media timestamp. Pairing outputs and draws by their positions in arrays would assign the wrong frame after a drop, so that approach was removed. Draw completion has its own timestamp; existing lateness and feedback calculations retain their prior scheduling timestamp.

Ordinary viewing requests SDK stats snapshots every 250 ms. It avoids constructing and freezing a stats object for each frame. Counters and presentation remain exact. Recording and the SDK's default stats mode retain per-presented-frame snapshots. This change has no isolated CPU benchmark yet.

## The capture experiment

The candidate replaces per-frame WLR screencopy negotiation with `ext-image-copy-capture-v1`. Full-output support was verified on the actual private labwc compositor, beyond the cursor support already in use.

It retains one compositor SHM mapping and three raw encoder slots. There is still one replaceable pending raw frame, and every submitted frame still incurs a full copy. Keeping ownership unchanged isolates the protocol experiment from the proposed shared-buffer pool.

Each persistent session receives constraints at creation and when they change. Capture marks the whole buffer damaged. Resize, media-generation changes, readiness loss and cursor-overlay changes replace the session so idle recovery can obtain a first frame without waiting for desktop damage. Non-normal transforms and unsupported formats fail explicitly. There is no WLR fallback or backend selector in the worktree.

Parent inspection and independent review corrected these problems before the final candidate build:

- The first prototype authorized another ext copy before copying the completed SHM image. Unlike WLR's initial negotiation request, that ext request can immediately allow another compositor write. Next capture now follows completed submission.
- A replacement constraint batch could cancel an outstanding frame and retain its same-sized buffer. Output and cursor capture now retire that storage. Destroying a frame does not provide the buffer-reuse guarantee of `Ready`.
- A failed encoder spawn discarded the only raw frame. A static ext session could then wait forever for damage. The worker now retains that frame unless newer input has arrived, waits before claiming pending input, and terminates after six consecutive spawn failures. Backoff still starts at one second and caps at 30 seconds.
- Cursor `Stopped` failures formerly retried the stopped session. They now terminate. Unknown and constraint failures have finite retry counts, reset by successful `Ready`.
- A delegated test change made cursor event output optional in production. Parent review removed that shape. Tests now use the real event encoder and bounded queue without a stdout thread.

Queued Wayland proxy tests exercise local requests and ownership. They do not run a compositor or prove all server event interleavings. The live runs cover the successful compositor path; injected encoder-spawn tests prove retry without another submission and preservation of newer pending input.

## Results

All paths below are under `results/`. Every run requested native 1824×848 motion at 60 Hz, an 8 Mbps encoder target and 60 ms browser presentation. Gates remained at 35 dB paused RGB PSNR, at least 45 canvas FPS, no freeze above 250 ms and zero decoder resets. Adaptive quality remained enabled; its reductions invalidate a fixed-quality comparison.

| Run                               | Canvas FPS | Longest freeze |     RGB PSNR | Result                                                               |
| --------------------------------- | ---------: | -------------: | -----------: | -------------------------------------------------------------------- |
| `performance-round3-wlr-stages`   |      49.75 | about 116.7 ms | 18.868122 dB | Failed paused fidelity; decoded image visibly stale                  |
| `performance-round3-wlr-confirm`  |  50.330817 |       116.8 ms | 35.727106 dB | Passed                                                               |
| `performance-round3-ext-single`   |  54.527154 |       133.3 ms | 35.477658 dB | Passed; exact gateway from WLR confirmation                          |
| `performance-round3-ext-confirm`  |  52.854686 |       816.7 ms | 35.687955 dB | Failed freeze and reset gates                                        |
| `performance-round3-ext-untraced` |  45.905918 |       610.3 ms | 32.923349 dB | Failed freeze, lateness, queue, generation, reset and fidelity gates |
| `performance-round3-wlr-restored` |  49.730018 |       119.0 ms | 35.697274 dB | Passed after restoring the exact earlier baseline pair               |

The first WLR failure compared a decoded chart showing 46.133 seconds with a stable native chart showing 46.700 seconds. That stale image does not establish a color-conversion regression. Its cause remains unresolved. The artifact is retained rather than replaced by the later passing run.

The matched WLR/ext pair isolates the daemon change. Capture sequence rate rose from 51.49 to 56.16 frames/s. Capture callback spacing p95 fell from 32.87 to 19.66 ms. Request-to-copy authorization fell from about 0.70 ms to effectively immediate, but the single-buffer candidate must first spend about 0.62 ms copying the previous image. Those intervals overlap differently in the two protocols; they cannot be added as independent savings.

The ext trace's protocol timestamp comes from `presentation_time`, while the WLR timestamp comes from its `Ready` event. Both use the host monotonic clock, but describe different protocol boundaries. Neither counts every distinct presentation by FFplay.

CPU did not improve in the matched pair. Median streamd use rose from about 17% to 20% of one core, encoder FFmpeg from 101% to 115%, and the FFplay workload from 232% to 251%. The eight-CPU Sprite was not saturated. More frames also produced more compressed traffic: about 6.91 Mbps became 7.52 Mbps. Higher rate or load could affect later delivery; the traces do not exclude that interaction.

The reviewed candidate retained roughly 57 captured frames/s during its failed traced run. Its gateway was rebuilt and has a different binary hash, so that repeat is not the exact earlier matched pair. The later untraced run started after an encoder-recovery check, incurred 17 browser long tasks and lowered bitrate during warmup and recording, eventually reaching 4000 kbps. It is not evidence that disabling native tracing caused the failure, nor a valid fixed-quality throughput comparison.

## Where the largest traced stalls appeared

`analyze-delivery.ts` joins each browser header to the gateway's completed write using generation and capture sequence. It compares adjacent-frame intervals measured separately on each clock. Subtracting those intervals avoids subtracting unrelated clock epochs; clock-rate skew remains uncalibrated.

In the initial WLR trace, frames 884 and 885 had these intervals:

- Capture timestamps: 16.61 ms apart.
- Gateway socket-write completions: 18.59 ms apart.
- Browser receipt callbacks: 115.40 ms apart.

In the failed ext confirmation, frames 1630 and 1631 had these intervals:

- Capture timestamps: 16.54 ms apart.
- Gateway socket-write completions: 17.12 ms apart.
- Browser receipt callbacks: 816.40 ms apart.

Neither interval overlapped a recorded browser long task. In the ext run, the subsequent burst reached the existing 24-frame decode queue limit and reset the decoder. The gateway's longest measured write took 5.52 ms; capture callback spacing never exceeded 22.42 ms.

The extra gap appeared after local socket acceptance. That boundary includes kernel buffering, remote ingress, the network, the local acceptance proxy and browser delivery. A successful async write does not prove bytes reached the browser. These observations do not identify which hop stalled, prove TCP loss, or establish physical input latency. The untraced run also shows that browser main-thread stalls can occur, so its failure should not inherit the traced run's narrower explanation.

The next useful delivery measurements are header-only arrival/forwarding times at the local proxy, browser-host CPU/event-loop profiling, and bounded TCP queue/retransmission observations. Those should precede a transport replacement or larger decoder queue. Shared SHM leases remain a separate capture experiment, not a remedy for an 800 ms downstream gap.

## Verification and deployment state

Local checks passed: `pnpm check:rust`, the four explicit real-FFmpeg tests, the real IPC/HTTP fixture suite, strict probe TypeScript, proxy tests and ShellCheck. The worktree has 82 passing Rust unit tests, 33 viewer tests and 17 recording/timing tests. Two throughput benchmarks remain ignored; the four FFmpeg tests were run separately.

On the reviewed ext candidate, native FFmpeg termination produced a replacement child and new SSRC. A subsequent browser smoke check rendered real pixels in two viewers and after a static reconnect. These checks do not prove uninterrupted presentation in an already-connected viewer. Performance probes also changed native output size and paused the native application through browser input. Held-input failure acceptance, cursor-shape acceptance and ordinary typing/scroll/window-motion workloads remain incomplete for this candidate.

Repeated acceptance failed, so the deployed trial returned to the saved WLR pair. Its restoring run passed all gates, with 49.73 canvas FPS, 15.26 ms p95 lateness, 119 ms longest freeze and zero resets. This does not erase the earlier WLR stale-frame failure or establish universally smooth delivery.

Installed and live hashes verified by `performance-round3-wlr-restored`:

```text
0c771df4c0fb48d99971efc3133b52a7b3f0dd0474812bd8cd3ba72ddaa6abb4 gateway
9d3260f9e2cca22a8aba464f5267602de5edf50080afc1fb76a94abbaef7c19f streamd
```

The deployment marker is `3876500908e6c1f18b84cef0a88f3e7211731a29`. It records deployment provenance, not a claim that current source builds the restored binaries. The gateway came from `target/performance-round3-ext/`, and the WLR daemon from `target/performance-round3-wlr/`. The WLR source was saved before migration, with snapshot `3f54ee99c5af84f8150a78066b5ef2b55f013c5f` as the baseline reference.

The reviewed, currently undeployed candidate is saved in `target/performance-round3-ext-reviewed/`:

```text
94604346c9207031e448156273e2c76517ea3e07b7cc21afb2b668cc815faeff gateway
2a7575f71fe0b3fbeb2ba4232a228131a1a8660bdd5ad936d944efa66c2c687b streamd
```

No commit was created. Temporary workload and browser processes were closed; the private desktop remains running.
