# Round eight: host-load comparison paused

On 2026-09-07, the first WLR recording failed. The host did not stay lightly
loaded, so the proposed WLR/ext/ext/WLR comparison stopped before the first ext
arm. This is an incomplete comparison, not a new verdict on persistent capture.

No binaries changed. The retained round-six WLR service was restarted after the
recording to clear adaptation and return to 8000 kbps. Installed and live hashes
matched, health passed, and owned test processes were cleaned up.

## Purpose and conditions

Round seven established that persistent capture delivered more distinct images,
but each capture implementation passed once and failed once. Fable proposed
repeating the comparison without competing host work, using the existing host
sampler to test the CPU-contention hypothesis. Its claim that the previous traces
proved an idle main thread was too strong: no long tasks does not mean idle, and
submission-to-output time includes waiting as well as decoding.

This attempt used the exact round-seven binaries, numbered workload, public
route, viewport, fidelity checks, recovery exercise and acceptance gates. It
added the existing `--delivery` option, which enables proxy, host and TCP
sampling. No new instrumentation or production code was added. No builds or
other test workloads were launched by this session during the recording, and no
other sessions were stopped or reprioritized.

Before the run, five one-second `vmstat` intervals showed about 6–14% user/system
CPU, but about 9–12% I/O wait. Aggregate pressure readings showed CPU and memory
`avg10` at 0%, while I/O `some avg10` was 34.58% and `full avg10` was 31.98%.
These were preflight readings, not measurements of Chrome waiting on I/O. The
host was therefore described as lightly CPU-loaded, not entirely idle, before
starting. Background work was not controlled.

```sh
pnpm exec tsx probes/rust-desktop/performance.ts \
  --sprite sprite-desktop-rust \
  --output probes/rust-desktop/results/performance-round8-wlr-1 \
  --route public --restart-trial --fidelity --exercise-recovery \
  --frame-counter --stages --delivery
```

## First WLR result

Run ID: `28ed2ead-e0b0-4a24-ac35-aca220a4eaca`.

| Measurement                     |                       Result |
| ------------------------------- | ---------------------------: |
| Distinct drawn FPS              |                        47.38 |
| Longest canvas freeze           |                     149.5 ms |
| Confident lateness p95          |                     96.63 ms |
| Decoder queue p95               |                            3 |
| Decode submission to output p95 |                      68.3 ms |
| Normal decoder resets           |                            1 |
| Quality during recording        | 8000 → 6400 kbps, 100% scale |
| Paused RGB PSNR                 |                  34.91848 dB |
| Host CPU median, all cores      |                       27.44% |
| Host CPU p95, all cores         |                       50.11% |
| Host CPU maximum, all cores     |                       56.86% |

The recording failed quality, generation, zero-reset and fidelity gates. Rate,
frame-gap, freeze, lateness and queue-p95 gates passed. The separate injected
recovery exercise passed; that does not rescue normal acceptance.

All 1422 recording draws had valid, distinct source IDs. There were no duplicate
IDs, regressions, invalid reads or counter overflow. Host sampling produced 29
samples with no errors or overflow. CPU rose to roughly 42–57% across all cores
around 16–20 seconds after host sampling began. The owned browser-driver process
tree used roughly 1.1–2.1 cores across the run; whole-host load was not just the
measured browser tree.

This was not CPU saturation across the machine. Nor do one-second aggregate
samples exclude brief or per-core contention. They establish that the intended
light-load condition did not hold steadily. They do not establish that competing
CPU work caused the decoder delay. No recording-time I/O attribution was
collected. The remaining three arms were not run, rather than repeatedly seek a
favorable window on an uncontrolled host.

## Retained state and evidence

The unchanged installed and live pair is:

```text
gateway 0c771df4c0fb48d99971efc3133b52a7b3f0dd0474812bd8cd3ba72ddaa6abb4
streamd 3341878c62241739569ec6b9b0e435afea1fba12050da5914040f5bca92bab14
```

The final definition-only restart returned the encoder to 8000 kbps, High 4:4:4,
GOP 15 and `-re`. Native health and installed/live hash checks passed at
`2026-09-07T09:16:52.852Z`. The browser and proxy closed, owned FFplay was terminated,
its remote log was removed, and local ports 3217/3218 were empty. The source
worktree still contains experimental ext capture; a default build does not
reproduce the deployed WLR daemon.

- [Raw first-run artifacts](./results/performance-round8-wlr-1/summary.json),
  including the failed checks and cleanup result.
- [Host/proxy/TCP observations](./results/performance-round8-wlr-1/delivery.json).
- [Pause decision and final live verification](./results/round8-paused.json).
- [Unchanged build provenance](./results/round7-builds.json).

No source tests were rerun for this artifact-only attempt. A new comparison needs
both repeats of both variants in the same controlled host window. This failed run
cannot supply its quiet-host WLR baseline, and it gives no new acceptance verdict
on ext.
