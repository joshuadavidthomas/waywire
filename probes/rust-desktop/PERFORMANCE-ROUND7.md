# Round seven: persistent capture delivers more distinct images, but does not pass twice

Completed on `sprite-desktop-rust` on 2026-09-07. The four planned recordings ran
in WLR/ext/ext/WLR order. Keep the exact round-six WLR deployment. Each capture
variant passed once and failed once; persistent capture did not meet the agreed
requirement to pass both runs. No extra recordings were run to seek two passes.

## Results

The workload remained fullscreen FFplay `testsrc2`, 1824×848 at nominal 60 FPS,
with a small binary source-frame number added to its pixels. Both arms used the
same encoder source, 15-frame keyframes, 1 MiB pipe, `-re`, 8 Mbps initial target,
full-scale High 4:4:4, 60 ms presentation target, and exact saved gateway/viewer.
Every recording explicitly restarted the owned service to clear adaptive history.

Directory suffixes below follow `results/performance-round7-`.

| Suffix  | Distinct drawn FPS | Capture sequence rate | Max freeze, ms | Decoder queue p95 | Normal resets | RGB PSNR, dB | Result          |
| ------- | -----------------: | --------------------: | -------------: | ----------------: | ------------: | -----------: | --------------- |
| `wlr-1` |              49.16 |                 51.62 |           86.1 |                 0 |             0 |       35.580 | Passed          |
| `ext-1` |              52.11 |                 58.85 |          118.3 |                16 |             3 |       33.022 | Failed; adapted |
| `ext-2` |              55.06 |                 58.59 |          119.3 |                 1 |             0 |       35.596 | Passed          |
| `wlr-2` |              48.96 |                 53.15 |          139.0 |                 4 |             1 |       35.017 | Failed; adapted |

The passing ext arm displayed 5.90 more distinct images per second than the
passing WLR arm, about 12%. All 6,161 recorded draws across the four arms had
valid counters, with zero duplicate IDs, regressions, or counter overflow.
The higher displayed rate is not repeated drawing of the same numbered image.
The failed arms are not fixed-quality comparisons: ext reached 5120 kbps during
recording and 4000 afterward; WLR reached 6400 kbps. Both passing arms held
8000 kbps/100% in every SDK sample.

Persistent capture also cost more CPU. In the passing arms, daemon median CPU
was 11.99% for WLR and 14.99% for ext; encoder CPU was 96.95% and 115.91%; the
FFplay workload was 234.87% and 256.81%. These percentages use one logical core
as 100%, on an eight-CPU Sprite. Actual payload was 7.01 versus 7.94 Mbps despite
the same 8 Mbps target. More captured frames therefore meant more encode,
decode, and transmission work.

The aggregate, cleanup records, exact hashes, and final health check are in
[round7-comparison.json](./results/round7-comparison.json). Original recordings,
traces, counter records, and screenshots remain in each result directory.

## What failed, and what this establishes

`ext-1` developed sustained decoder pressure. Decode-submission-to-output p95
was 279.3 ms, confident presentation lateness p95 was 296.6 ms, and the decoder
queue p95 reached 16. Quality reductions changed the generation, and one reset
also hit the hard queue cap. The run failed lateness, queue, stable generation,
fixed quality, zero resets, and fidelity. There were no browser long tasks.

`wlr-2` also failed. Decode-submission-to-output p95 was 86 ms and confident
lateness p95 was 105.4 ms. Quality fell to 6400 kbps and caused a generation
change/reset. Two browser long tasks of 56 and 58 ms were recorded. It failed
lateness, stable generation, fixed quality, and zero resets. Fidelity narrowly
passed, but that does not repair the other failures.

All four passed the existing 250 ms maximum-freeze gate. These failures therefore
cannot be described simply as one oversized freeze landing in a short run.
The browser decoding and presentation pipeline fell behind during the failed
arms. This round did not sample browser-host CPU or add proxy delivery timing,
so it does not establish why. Neither a platform-only explanation nor a claim
that persistent capture caused the pressure follows from these observations.

We have confirmed an improvement in distinct delivered images with persistent
capture on this workload. We have not established repeat acceptance for either
variant, the cause of the decoder pressure, or sustained 60 distinct FPS. The
counter labels FFmpeg-generated images; it does not enumerate every compositor
presentation or locate where each missing source ID was skipped.

## Separate recovery exercise

The unchanged post-recording exercise injected one queue-size observation of 24
and verified the actual reset, next keyframe, decoder output, and draw identities.
It passed in all arms. Reset-to-draw was 323.0/203.1/264.6/314.4 ms in run order.

The failed ext arm already had 13 real queued chunks when the synthetic read
fired, and had adapted its quality. Its 203.1 ms result is not a matched recovery
win. The other three injections observed an actual queue of zero. None of these
exercises changes the normal zero-reset gate or gives a wall-clock recovery bound.

## Counter implementation and review

`--frame-counter` adds a 288×8 strip containing 16 bits, inverse cells, and
white/black guards. FFmpeg's timeline `n` selects the cells. The browser draw hook
reads a separate 36×1 `willReadFrequently` canvas, just 144 RGBA bytes per draw;
it does not read back the full display canvas. At most 10,000 numeric records are
retained. Missing timestamps, unreadable pixels, damaged guards/complements, and
overflow are explicit failures.

Pre-live review caught a mismatch between an independent 30-second observer
cutoff and the recorder's actual lifetime. The counter now follows the local
recorder's own `drawImage` wrapper, including a delayed final timer. Its gate also
requires the counter record count to equal `recording.canvasSubmitsMs.length`.
A test covers draws before recording, after 30 seconds while still recording,
and after recorder cleanup. All four live runs matched those counts exactly.

Both variants used the same counter workload and read hook. The hook adds work,
and its runtime cost was not separately measured. These numbers describe the
instrumented trial, not a promise of identical performance without it.

## Source, rollback, and cleanup

The WLR binary was reused without rebuilding. The isolated ext source lives at
`target/performance-round7-ext/source`. It starts from the saved WLR source at
`3f54ee99c5af84f8150a78066b5ef2b55f013c5f` plus the retained
[round-six encoder patch](./round6-wlr.patch), then applies
[round7-capture.patch](./round7-capture.patch).

Only the reviewed capture integration, cursor integration, capture timing events,
and a test-only event-sink helper differ. In particular, `video.rs` is byte-for-byte
identical between these two source trees. The main worktree's later idle encoder
spawn-retry fixes were not mixed into one arm. The reviewed ext output failure
policy remains fail-fast for unknown protocol failures; it was not broadened in
this experiment. The single SHM mapping is copied into the encoder pool before
another compositor write is authorized.

[round7-builds.json](./results/round7-builds.json) records the assembled source-file
hashes. `SOURCE-REVISION` in the exported tree identifies its base, not the full
patched candidate. Binary identities are:

```text
shared gateway 0c771df4c0fb48d99971efc3133b52a7b3f0dd0474812bd8cd3ba72ddaa6abb4
retained WLR   3341878c62241739569ec6b9b0e435afea1fba12050da5914040f5bca92bab14
trial ext      bb9abf1857b219d67d61715770a3e8c8b2cac475d9d4bcb30bc212bb49a0cf79
```

Two initial native preflight attempts found no HTTP listener before any round-seven
deployment. The owned service definition and installed WLR hashes matched; a
definition-only restart restored health. The cause of that initial unavailability
was not investigated in this capture trial.

After the fourth recording, a final definition-only restart cleared WLR's adapted
quality. The installed/live WLR and gateway hashes matched and native health was
`ok` at `2026-09-07T02:43:56.237Z`. FFmpeg was back at 8000 kbps, 15-frame keyframes,
and `-re`. All four browsers, proxies, owned FFplay processes, and remote scratch
logs were cleaned up. Local ports 3217 and 3218 had no listener.

The main worktree still builds experimental ext capture. No production source was
changed by this round; its retained changes are probe code and evidence. Building
and deploying the main worktree would not reproduce the installed WLR bytes.
Protected Sprites, comparison services, and original recordings were untouched.

## Checks and repetition

The isolated candidate passed its release build, Clippy, 48 native unit tests,
and four explicit real-FFmpeg tests. Six tests were ignored by its default test
run; four were then run explicitly, and two throughput benchmarks remained
unperformed. The main worktree passed `pnpm check:rust`: 83 Rust unit tests,
33 viewer tests, 56 probe tests, viewer build, formatting, and Clippy. Its seven
default-ignored native tests were not rerun against the main source. Strict probe
TypeScript, the three real proxy tests, and ShellCheck also passed. IPC was not
rerun for these probe-only changes; it passed in round six. Broader native failure
acceptance was not repeated, and no candidate was retained.

The same flags were used for all four arms, with a unique directory each time:

```sh
pnpm exec tsx probes/rust-desktop/performance.ts \
  --sprite sprite-desktop-rust --output NEW_DIRECTORY \
  --route public --restart-trial --fidelity --exercise-recovery \
  --frame-counter --stages
```

This command interrupts the owned session. Deployment requires separately
authorized explicit binary paths; never use a default main-worktree build to
restore this WLR baseline.
