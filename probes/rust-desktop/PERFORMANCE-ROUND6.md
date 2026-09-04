# Round six: keep shorter keyframe recovery and a larger pipe

Trials ran on the private `sprite-desktop-rust` service on 2026-09-06–07.
The retained changes are a 15-frame keyframe interval at nominal 60 FPS and a
1 MiB FFmpeg stdin pipe. FFmpeg's `-re` remains. Capture remains WLR, and every
arm used the exact same saved gateway/viewer.

## Results and decision

All directories below start with `results/performance-round6-`.
CPU is the median process usage, with **one logical core equal to 100%**.
The Sprite has eight logical CPUs. Recovery is a separate forced-reset exercise,
measured from the real decoder reset to the first recovered canvas draw.

| Directory suffix | Canvas FPS | Normal max freeze, ms | RGB PSNR, dB | Daemon CPU, % of one core | Forced recovery, ms | Normal acceptance |
| ---------------- | ---------: | --------------------: | -----------: | ------------------------: | ------------------: | ----------------- |
| `gop60`          |      49.03 |                 166.8 |       35.705 |                     16.99 |              1164.1 | Passed            |
| `gop30`          |      48.66 |                 116.7 |       35.781 |                     16.99 |               596.9 | Passed            |
| `gop15`          |      48.79 |                 150.1 |       35.806 |                     16.99 |               108.9 | Passed            |
| `gop15-confirm`  |      48.83 |                 116.8 |       35.713 |                     16.99 |               326.2 | Passed            |
| `pipe1m`         |      48.86 |                 150.0 |       35.786 |                     11.99 |               314.0 | Passed            |
| `no-re`          |      50.06 |                 116.6 |       34.791 |                     11.99 |               282.8 | Failed            |
| `pipe1m-confirm` |      49.29 |                  99.9 |       35.987 |                     11.99 |               312.8 | Passed            |

The first three arms changed only the keyframe interval. `pipe1m` added pipe
capacity to the 15-frame variant. `no-re` then removed input pacing. The last arm
restored the exact `pipe1m` bytes and confirmed that configuration.

Keep the 15-frame interval: it reduced the near-worst-phase keyframe wait without
failing the existing fidelity gate. Keep the larger pipe: its repeated daemon
CPU reduction was about five percentage points of one core, or about 29% of
that process's CPU use. Encoder CPU also fell from roughly 99–100% to 96% of one
core. These are modest whole-machine savings; no throughput gain is established.

Reject `-re` removal for this trial. At about 27 seconds, quality adapted to
6400 kbps, the generation changed, and the decoder reset once. Fidelity then
failed. The run does not prove that removing `-re` caused the pressure, and its
higher FPS is not a fixed-quality win. Its artifacts remain unchanged.

Every passing normal recording held 8000 kbps and 100% scale in every SDK sample,
kept one generation, had zero decoder resets, and passed the existing frame-gap,
queue, clock-confidence, and 35 dB fidelity gates. No thresholds changed. Paused
chart images differ across trials, so PSNR variation is not a measured fidelity
improvement. Full chroma, resolution, codec settings, and the 60 ms presentation
target stayed fixed.

## Controlled recovery, separate from normal acceptance

`performance.ts --exercise-recovery` runs after the ordinary recording and timing
collection finish. It waits for a plain keyframe, then returns 24 for one
`decodeQueueSize` read on the next delta. The unchanged SDK takes its actual
queue-cap branch, resets the real browser decoder, and waits for a keyframe.
The real queue was zero at injection in these runs. This exercises recovery; it
does not simulate a decoder genuinely struggling under CPU contention.

The hook has one attempt and a 10-second lifetime. It records header identity,
actual reset, decoder output, and canvas draw in `recovery.json`. The injected
reset never enters the normal 30-second recording. Failure of the separate
exercise fails the combined run; it cannot turn a failed normal recording into
a pass.

Parent and independent review tightened the assessor to require an installed
draw hook, the same media timestamp for the next keyframe, first decoded output,
and first draw, plus correct event order and generation. All seven saved exercises passed those
stricter checks without rewriting their original artifacts. See
[the comparison and recheck](./results/round6-comparison.json).

The first 15-frame exercise had bunched arrivals: about 300 ms of source time
between keyframes arrived about 102 ms apart in the browser. Its 108.9 ms recovery
is an observation, not a bound. The repeat and both retained-pipe runs showed
313–326 ms. An upstream stall or slow decoder can still extend recovery. A
frame-count interval also gives no wall-clock guarantee on an idle desktop.
These changes do not establish universal smoothness or sustained 60 FPS.

## Desktop check

A separate ordinary viewer connected to the retained deployment. Physical CDP
key events typed `round six typing works` in an owned FeatherPad document and
saved it; a native file read verified the text. Browser screenshots showed the
sentence, scrolling from line 1 to line 25, and a moved editor window. Control
acquisition resized the desktop from 1824×848 to 1232×448 and retained 8 Mbps.

The first attempt through `agent-browser keyboard type` did not produce the
expected saved sentence. That attempt is not a pass. The later physical CDP
sequence produced the verified result. Immediate screenshots also preceded some
remote updates; the settled screenshots provide the visible evidence.

Evidence is `results/round6-desktop-{sentence,scroll-settled,moved}.png` and the
other retained `round6-desktop-*.png` captures. These are functional checks, not a
quantitative typing, scrolling, or input-to-photon benchmark. The owned editor,
synthetic document, log, browser, and acceptance proxy were cleaned up.

## Build and deployment identity

The native trial sources came from jj snapshot
`3f54ee99c5af84f8150a78066b5ef2b55f013c5f`, exported to
`target/keyframe-trial-20260906/source`. The saved WLR capture files matched that
snapshot byte for byte. A fresh 60-frame baseline was built before changing it;
its bytes differ from the older deployed daemon. Every trial restarted only the
owned service to clear adaptive history and verified changed PIDs and the
expected binary hashes.

[round6-wlr.patch](./round6-wlr.patch) records the retained change against that
historical source. The saved source directory now contains this retained variant;
rebuilding it reproduced the deployed hash exactly:

```sh
cargo build --locked --release -p sprite-desktop-streamd \
  --manifest-path target/keyframe-trial-20260906/source/Cargo.toml \
  --target-dir target/keyframe-trial-20260906/build
```

The deployment now uses:

```text
gateway 0c771df4c0fb48d99971efc3133b52a7b3f0dd0474812bd8cd3ba72ddaa6abb4
streamd 3341878c62241739569ec6b9b0e435afea1fba12050da5914040f5bca92bab14
```

Saved binaries are `target/performance-round3-ext/sprite-desktop-gateway` and
`target/performance-round6-pipe1m/sprite-desktop-streamd`. All variant hashes and
the original rollback paths are in [round6-builds.json](./results/round6-builds.json).
The deployment marker's jj revision records upload provenance, not the native
source revision.

The successful changes were also applied to `crates/streamd/src/video.rs` in the
main worktree. **That worktree still uses experimental ext capture.** Building and
deploying it would replace the tested WLR capture implementation. This round did
not deploy or establish acceptance for ext. No gateway/viewer binary was rebuilt
for the live trials, and protected Sprites and original recordings were untouched.

The 1 MiB pipe allocation is required, not silently downgraded. If the kernel
rejects it, startup closes stdin and kills/reaps the encoder child through the
existing failure path. The owned Sprite and local real-FFmpeg tests accepted it.

## Verification

- Isolated WLR candidate: 39 unit tests, four explicit real-FFmpeg tests, release
  build, formatting, and Clippy passed during the trials.
- Main worktree: `pnpm check:rust` passed: 83 Rust unit tests, 33 viewer tests,
  50 probe tests, viewer build, formatting, and Clippy. Seven native tests were
  explicitly ignored by the default gate.
- `pnpm test:rust:media` separately passed all five real-FFmpeg tests, including
  the new actual-pipe-capacity assertion. The two throughput benchmarks remain
  unperformed.
- `pnpm test:rust:ipc`, strict probe TypeScript, all three real proxy tests, and
  ShellCheck passed.
- Final native health/live-hash check passed at `2026-09-07T00:04:00.715Z`.
  Local ports 3217 and 3218 had no listener after cleanup.

To repeat a normal recording plus the separate recovery exercise, use a new
output directory and the same explicit reset on every comparison arm:

```sh
pnpm exec tsx probes/rust-desktop/performance.ts \
  --sprite sprite-desktop-rust --output NEW_DIRECTORY \
  --route public --restart-trial --fidelity --exercise-recovery
```

This command recreates the owned service definition and interrupts its session.
Do not run another deployment or live trial concurrently.
