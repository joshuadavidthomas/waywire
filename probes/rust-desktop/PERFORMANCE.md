# Rust desktop performance corrections

First- and second-round measurements on the private `sprite-desktop-rust` trial
on 2026-09-06. [Round three](./PERFORMANCE-ROUND3.md) adds timing evidence and a
persistent-capture experiment. Repeat acceptance failed, so WLR capture was
restored. [Round six](./PERFORMANCE-ROUND6.md) later retained a shorter keyframe
interval and larger FFmpeg pipe on WLR. The worktree still builds ext capture,
now with those encoder changes. VNC, Waymote and their original recordings
remain unchanged. [Round four](./PERFORMANCE-ROUND4.md) adds local proxy, browser-host
CPU and TCP measurements without changing those binaries. It locates delays on
both sides of the proxy, but neither live run passed full acceptance.
[Round five](./PERFORMANCE-ROUND5.md) compares public HTTPS, a Sprite tunnel and
public HTTPS again at verified 8 Mbps. All three recordings passed; the tunnel
showed no improvement. [Round seven](./PERFORMANCE-ROUND7.md) verified a real
increase in distinct delivered images with persistent capture, but both capture
variants failed one of their two runs. The round-six WLR build remains deployed.
[Round eight](./PERFORMANCE-ROUND8.md) paused a host-load comparison after its first
WLR recording failed and host CPU rose during the run. No binaries changed; the
remaining three arms were not run.

## Follow-up critical-path changes

The second pass implemented all six recommendations without changing the encoder,
color profile, bitrate target, presentation target, IPC framing or process split:

1. Request and flush the next capture before copying the completed frame. This
   overlaps constraint negotiation with the copy. The compositor cannot reuse the
   SHM mapping until a later dispatched `BufferDone` authorizes the next copy.
2. Wake calloop when the encoder publishes a notification, replacing the 10 ms
   polling timer. The notification queue stays bounded at 64 entries; fatal
   errors also wake the loop when that queue is full.
3. Enable `TCP_NODELAY` on accepted gateway sockets. This changes only the
   gateway's TCP hop, not Sprite ingress or the browser proxy.
4. Keep an arriving keyframe when a viewer queue overflows. Mark it as recovery
   rather than discard it and wait for another GOP.
5. Recover quality relative to received frames instead of requiring 54 rendered
   FPS from a source supplying about 51 FPS. Feedback now carries interval
   `received`, `presented`, `queuePeak`, `queueBusyMs`, `sampleMs`, `dropped` and
   `rtt`; the old feedback shape is rejected. Idle intervals do not adapt.
6. Update the human metrics footer at 4 Hz without throttling recorder samples;
   refresh cursor geometry on relevant changes rather than every draw. A separate
   bounded control writer lets reads continue while output stalls. Queue overflow
   disconnects rather than silently dropping cursor or clipboard state, and lease
   release precedes the bounded writer join.

Review also found that decoder reset could leave old decoded frames scheduled.
Reset now closes those frames and cancels their presentation work. Tests cover
both hard overflow and generation/discontinuity recovery.

### Live results and the rejected quality heuristic

All runs below used the same workload and unchanged acceptance thresholds as the
first pass. Paths are under `results/`.

| Run                                 | Canvas FPS | Longest freeze | Paused RGB PSNR | Result                                              |
| ----------------------------------- | ---------: | -------------: | --------------: | --------------------------------------------------- |
| `performance-round2-early`          |      49.16 |       133.4 ms |        35.47 dB | Passed initial implementation                       |
| `performance-round2-late-ablation`  |      49.72 |       116.7 ms |        34.36 dB | Failed fidelity and generation/reset gates          |
| `performance-round2-late-pressure`  |      50.06 |       133.4 ms |        35.44 dB | Passed corrected quality policy, late request       |
| `performance-round2-early-pressure` |      49.13 |       116.8 ms |        35.77 dB | Passed; gateway rebuilt, not the matched comparison |
| `performance-round2-early-matched`  |      50.66 |       116.7 ms |        35.33 dB | Passed with the exact late-pressure gateway binary  |
| `performance-round2-final`          |      50.96 |       166.6 ms |        35.67 dB | Passed same deployed pair, stage recording off      |

The first late-request experiment exposed an adaptation regression. Treating a
brief queue peak of five as a bad interval lowered the bitrate from 8000 to
6400 kbps, changed generation and failed fidelity. That run does not show a
capture-ordering regression. The corrected policy measures time observed with at
least five queued decode requests, including `dequeue` events. Two intervals with
at least 10% observed queue pressure, a peak reaching the hard limit of 24, or RTT
above 250 ms trigger reduction. Eight good intervals can restore quality: pressure
below 5%, no drops, peak below 24, RTT below 120 ms and at least 90% of received
frames presented. Changes remain at least five seconds apart. These thresholds
are a tested policy, not a measured universal congestion boundary.

The four corrected-policy runs kept one generation, no decoder resets, no
missing received capture sequences and the 8 Mbps target. Final p95 frame gap was
33.4 ms and p95 lateness was 15.93 ms. Monotonic counters classified 35 discarded
frames as overdue, with no decoded-output overflow or reset drops between the
first and last recorder samples. Those counter bounds differ slightly from the
recording's exact edges.

### What the matched capture experiment establishes

`late-pressure` and `early-matched` used the identical gateway binary and workspace
release build configuration. Only the streamd capture request/flush ordering
changed. Both collected bounded native stage traces.

| Stage interval                                | Late request p50 / p95 | Early request p50 / p95 |
| --------------------------------------------- | ---------------------: | ----------------------: |
| Ready callback to next request                |       0.658 / 0.770 ms |      0.0068 / 0.0086 ms |
| Ready callback to successful flush            |       0.670 / 0.783 ms |        0.016 / 0.024 ms |
| Pixel submission/copy                         |       0.650 / 0.759 ms |        0.631 / 0.755 ms |
| Complete raw pipe write                       |       3.855 / 7.988 ms |        3.756 / 7.320 ms |
| Notification publication to metadata dispatch |       0.046 / 2.616 ms |        0.047 / 2.616 ms |

Early requesting removes about 0.65 ms from the next-request path without adding
buffers. It does not establish a sustained FPS gain: other early runs ranged
from 49.13 to 50.96 FPS. Capture callbacks still alternate mostly between roughly
16 and 33 ms spacing. In the matched early run, compositor Ready to local callback
was 0.53 ms median and 1.02 ms p95. Source presentation, capture negotiation and
compositor damage remain candidates for the missing refreshes.

The notification measurements describe the new wakeup path; no matched old-timer
run was made. There is also no isolated live attribution for NODELAY, footer
throttling or cursor geometry caching. Recovery-keyframe and stalled-control
behavior have regression tests, including real socket pressure and native-peer
input receipt. The final ordinary run used about 2%, 17% and 107% of one core for
gateway, streamd and FFmpeg. This pass did not demonstrate a whole-pipeline CPU
reduction or sustained 60 FPS.

### Instrumentation, identity and checks

`performance.ts --stages` records native protocol Ready time, callback entry,
next-request completion, successful flush, submission, raw pipe writes,
notification publication and metadata dispatch. The launcher supplies a unique
private trace path. Streamd creates it exclusively with mode 0600; SIGUSR1 starts
or resets a bounded 32,768-record memory buffer and SIGUSR2 stops and dumps it.
The probe validates the owned daemon before signals and rejects stale prior dumps.
No frame payload, input or clipboard content enters the trace. Recording is off
normally; serialization and file writes happen outside the record mutex.

The parser keeps the first request and first successful flush when readiness or
cursor changes legally retry the same sequence, and counts those retries.
Unmatched stage records can arise at recording boundaries or from cancelled work.
The matched early trace had 14,679 records, no overflow and 1,553 browser matches.

The final installed and live binaries matched these SHA-256 values:

```text
48638a80171eb321afd9dc832be59b4a3f5d7d10e1781bd93edac35950cfb4f0 gateway
e99b4660e2a890f588ce6ad5e7e5704c2f6045b9c27bc38a08999c8fd567df53 streamd
```

The deployment marker is jj revision
`470a18ce38a16761db2361758065ad799442498e`. The late ablation used streamd hash
`7b4b4d8b70f61be18c1630ff0a9bf6ec2886f16c0486e20dcfb82abb94880086`.
The final source retains early requesting; temporary late binaries and source
are saved under ignored `target/performance-20260906-late-pressure/`. Subsequent
local changes only hardened the trace parser/types and updated documentation.

Final checks passed: 63 Rust unit tests, four explicitly selected real-FFmpeg
tests, 31 viewer tests, seven timing/recording tests, IPC/HTTP fixtures, strict
viewer and probe TypeScript, Clippy, formatting, the proxy test and ShellCheck.
The two ignored throughput benchmarks were not rerun. Parent inspection and a
separate final review checked queue accounting, control ownership, notification
wakeup and SHM safety. Review found a valid-recapture trace rejection; the parser
and regression test now cover it. Broader compositor failure acceptance remains
unfinished as listed below.

## Earlier performance corrections

Each run used 30 seconds of native fullscreen FFplay `testsrc2` at 1824×848,
requested 60 Hz, with an 8 Mbps encoder target and the browser's 60 ms presentation
target. Headless Chrome 152 on Linux received the real Sprite desktop through the
credential-isolating proxy. FPS counts canvas updates, not a cached SDK label.

All artifact directories below are under `results/`.

| Run                                   | Canvas FPS | Frame gap p95 | Lateness p95 | Paused RGB PSNR | Result                                     |
| ------------------------------------- | ---------: | ------------: | -----------: | --------------: | ------------------------------------------ |
| `performance-before-poll-fix`         |       4.80 |      233.4 ms |     240.5 ms |    Not measured | Failed                                     |
| `performance-after-poll-fix`          |      44.76 |       33.4 ms |       8.5 ms |    Not measured | Failed FPS floor                           |
| `performance-poll-stage-profile`      |      45.96 |       33.4 ms |       8.1 ms |        23.45 dB | Failed fidelity                            |
| `performance-pooled-color-scheduling` |      51.26 |       33.4 ms |      14.1 ms |        29.62 dB | Failed fidelity                            |
| `performance-full-chroma`             |      49.66 |       33.4 ms |      15.9 ms |        34.40 dB | Failed fidelity; also had an 817 ms freeze |
| `performance-superfast-burst-bound`   |      49.86 |       33.4 ms |      15.7 ms |        35.48 dB | Passed                                     |
| `performance-confirmation-1`          |      50.56 |       33.4 ms |      16.0 ms |        35.76 dB | Passed                                     |
| `performance-final-checked`           |      49.89 |       33.4 ms |      15.8 ms |        35.66 dB | Passed strengthened gate                   |

The final run includes recording-edge freezes and requires zero decoder resets.
Its longest freeze was 133.4 ms. All three final-settings runs observed zero
decoder resets, zero discontinuity flags, one generation and no missing capture
sequences between the first and last received headers.

Lateness is estimated time past the 60 ms presentation target, with about
28 ms clock uncertainty. It is not physical input latency. PSNR compares a paused
chart with the decoded canvas between two identical native screenshots. It does
not measure motion fidelity, every kind of desktop text, or losslessness.

The final run received 51.37 frames/s and drew 49.89 updates/s. It did not sustain
60 FPS. The remaining capture-rate limit has not been isolated. Gateway, streamd,
FFmpeg and labwc used approximately 2%, 17%, 104% and 7% of one CPU core at the
median. The separate FFplay workload used 233%; the Sprite has eight CPUs.
Browser CPU and hardware-decoder use were not measured.

## What changed in the first pass

1. The Rust FFmpeg writer slept 2 ms after every full pipe. A roughly 6.3 MB frame
   required about 97 pipe fills, accumulating about 200 ms. It now polls for
   writability, wakes as soon as the pipe can accept bytes, and retains stop,
   generation and stall checks. A real 64 KiB pipe test fell from 200.8 ms to
   6.7 ms per frame; that test excludes encoding and transport.
2. Capture copied into a new vector for each frame. The three encoder slots now
   retain their buffers. Tests check stable allocations and prevent pending-frame
   replacement from overwriting the frame being encoded. The live runs do not
   isolate the pool's performance contribution from the other changes.
3. The inherited browser scheduler treated frames up to 8 ms in the future as
   due. It could discard an already-due frame and draw its replacement early.
   It now presents the newest frame actually due and keeps future frames for
   later animation callbacks. Controlled-clock tests reproduce the old loss.
4. Unspecified color conversion produced large RGB errors. The stream now uses
   explicit full-range BT.709 matrix conversion, sRGB transfer metadata and full
   4:4:4 chroma. `superfast` replaced `ultrafast` to improve compression at the
   unchanged bitrate target. These are deliberate encoding changes, not a claim
   that the original baseline profile had a Rust-specific bug.
5. A short network batch could exceed six queued decode requests. The SDK reset
   a usable predictive stream and discarded deltas until the next keyframe. In
   `performance-full-chroma`, a roughly 121 ms receipt pause led to an 817 ms
   presentation freeze while subsequent headers kept arriving. The decoder now
   admits up to 24 queued requests. Hard overflow and discontinuities still reset
   it; decoded output remains separately bounded at 24 frames. Tests cover short
   bursts, the exact limit and keyframe recovery.

## Browser and codec boundary

The private trial now advertises `avc1.F40034`: H.264 High 4:4:4 Predictive, level
5.2. The real-FFmpeg RTP test checks the emitted SPS profile, constraint and level
bytes, chroma format, range, primaries, transfer and matrix. The gateway advertises
the matching codec string.

Only Chrome 152 on Linux was verified. Other browsers may reject this profile,
and hardware decoders may not support it. A browser may choose software decoding;
the probe does not establish which decoder ran. The SDK checks
`VideoDecoder.isConfigSupported()` and reports an unsupported codec. There is no
baseline-profile fallback or profile negotiation. This is a private-trial support
limit, not broad browser parity.

## First-pass evidence and remaining review limits

`performance-final-checked/summary.json` records hashes of both installed binaries
and their live `/proc/PID/exe` images, checked against the deployment marker. The
gateway embeds the viewer, so its hash identifies those assets too. This records
what ran; it is not a reproducible-build proof. The deployment was stamped with
jj revision `a8da8807f069d3422726db5b0e0a6a1d138dbd02`.

The earliest `performance-baseline-current-deployed-old2ms-*` attempts used an
agent-browser domain wrapper that removed WebSocket's static state constants.
That broke SDK control-state checks and clock confidence. Those failed attempts
remain as evidence, but the table uses the corrected `performance-before-poll-fix`
baseline. The fixed local proxy does not use that wrapper; the probe explicitly
checks the native WebSocket constants before recording.

Parent review checked the delegated pool and scheduling changes against source.
Further reviews caught omitted recording-edge freezes, a missing decoder-reset
gate and the need to document the new codec's support limit. Those were corrected.
The native SPS contract has a real-RTP test rather than only argument assertions.

Final local validation passed: 44 Rust unit tests, four real-FFmpeg tests, 23
viewer tests, three recording-boundary tests, strict TypeScript, Clippy, formatting,
the proxy test and ShellCheck. Repeated IPC checks exposed two fixture races: an
unsolicited event flood exceeded the subscriber limit before the blocked-input
case started, and rapid lease release could correctly cancel keys the test
expected to observe. The fixture now starts events after a confirmed large command
and checks receipt of a fresh event while stdin is blocked. The handoff case waits
for each key to reach the peer before releasing its lease. Four consecutive IPC
runs then passed; the batch timeout interrupted a fifth, which is not counted.

The broad fidelity audit and combined held-input, resize-failure and whole-service
recovery acceptance remain unfinished. These performance results do not close
those checks or establish sustained 60 FPS.
