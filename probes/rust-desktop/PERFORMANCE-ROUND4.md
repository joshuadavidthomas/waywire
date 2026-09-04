# Locating delivery stalls

[Round five](./PERFORMANCE-ROUND5.md) carries out the proposed public/tunnel/public
comparison at verified 8 Mbps. All three recordings passed, but delivery pauses
remained and the tunnel showed no improvement.

On 2026-09-06, two diagnostic runs used the saved WLR deployment without rebuilding, restarting or replacing its binaries. Both failed full acceptance. The new evidence separates delays before the local proxy from delays inside the browser host. It does not establish a performance fix.

The native daemon remains `9d3260f9e2cca22a8aba464f5267602de5edf50080afc1fb76a94abbaef7c19f`; the gateway remains `0c771df4c0fb48d99971efc3133b52a7b3f0dd0474812bd8cd3ba72ddaa6abb4`. Both installed and live hashes were verified. The ext capture candidate remains undeployed. VNC, Waymote and other sessions' processes were untouched.

## What was measured

The optional `--delivery` probe observes raw upstream WebSocket bytes before the acceptance proxy forwards them. Its parser handles split headers, coalesced frames, extended lengths, binary continuations and interleaved control frames. It retains only the fixed video header, frame size and first-byte/completion timestamps. It neither decodes nor stores the compressed payload. Malformed framing stops observation without interrupting forwarding.

Each connection retains at most 10,000 frame records, with at most four observed connections. A frame whose WebSocket header began before recording is skipped. The real TLS proxy test covers bytes arriving with the upgrade response and confirms that control traffic never reaches the observer.

The local sampler follows only the unique agent-browser driver's descendants. It records CPU use from `/proc`, identifies PID/TID reuse through start times, and samples host-wide CPU separately. It also records Node heartbeat delay every 100 ms. All records are bounded; finish cancels timers and joins pending collection. Other processes' arguments and environments are never read. No unrelated process was stopped or reprioritized.

The second run also sampled the proxy's owned upstream TLS socket using `ss`, about every 307 ms including command execution. Only numeric queue and TCP fields enter the artifact. Addresses, HTTP headers, credentials and packet data do not. Those receiver-side samples cannot report the remote video sender's retransmissions.

## Results and limits

Artifacts are under `results/`:

| Run                               | Canvas FPS | Longest freeze | Resets |     RGB PSNR | Outcome                                                            |
| --------------------------------- | ---------: | -------------: | -----: | -----------: | ------------------------------------------------------------------ |
| `performance-round4-wlr-delivery` |  43.928794 |      1062.9 ms |      3 | 34.353635 dB | Failed; local CPU pressure and adaptive reduction during recording |
| `performance-round4-wlr-tcp`      |  50.430476 |        83.4 ms |      0 | 34.473764 dB | Failed fidelity; started and stayed at 6400 kbps                   |

The first run collected 1,574 proxy frames and 28 host samples with no observation errors or overflow. The second collected 1,602 proxy frames, 29 host samples and 101 TCP samples with no observation errors or overflow. Proxy counts include a slightly wider window than the browser's exact 30 seconds; analysis joins actual frame identities rather than subtracting the counts.

The second run used the bitrate retained from the first run's adaptation. Its smoother motion cannot be compared as an 8 Mbps result. Review caught that the probe checked topology stability but did not enforce its stated starting bitrate. The probe now requires 8000 kbps before recording and checks every recorded SDK sample for 8000 kbps at 100% scale. The existing artifacts remain failures; they were not reclassified after that correction. A future fixed-quality comparison must restore the expected starting state, rather than silently measure a reduced bitrate.

## Delay before the local proxy

In the first run, generation 2 frames 2773 and 2774 had these intervals:

| Boundary                          |  Interval |
| --------------------------------- | --------: |
| Native capture timestamps         |  17.05 ms |
| Gateway socket-write completions  |  18.73 ms |
| Completed messages at local proxy | 274.81 ms |
| Browser receipt callbacks         | 270.80 ms |

The second message took 207.52 ms to arrive from its first WebSocket byte to its final payload byte. The Node heartbeat recorded only about 0.48 ms delay during the proxy gap. There was no overlapping browser long task. This delay had already accumulated before local forwarding.

The TCP run also showed pre-proxy stalls without browser long tasks. For generation 3 frames 6320 and 6321, gateway writes were 19.90 ms apart, proxy completions 91.38 ms apart and browser callbacks 91.40 ms apart. The message itself spanned 29.83 ms at the proxy.

This narrows the investigation to the path between gateway socket acceptance and the proxy's TLS data callback. That includes the gateway kernel, Sprite ingress, the network and the receiving network/TLS stack. It does not name the slow component or prove TCP packet loss.

The second run's largest sampled local receive queue was 3,259 bytes. The TCP samples bracketing the cited 91 ms gap were 615 ms apart, with zero queued bytes at both endpoints. Retransmission and out-of-order fields were unreported. These sparse observations neither locate a loss event nor exclude a short queue between samples.

## Delay inside the browser host

During the first run, host CPU samples covering roughly 18–22 seconds reached 96.61%, 97.22% and 90.26% across all cores. Around that interval the owned browser/driver tree's CPU use fell from about one core to 0.56 of a core, while decoder delays and resets increased. CPU pressure is a plausible contributor; these samples do not identify the competing process or prove causation for every delayed frame.

One directly joined interval provides stronger location evidence. Generation 3 frames 3712 and 3713 completed at the proxy 16.51 ms apart, while browser receipt callbacks were 107.10 ms apart. That browser interval overlapped a long task. The extra 90.59 ms appeared after the proxy had the complete messages.

In the later TCP run, host-wide sampled CPU stayed roughly between 16% and 41%. There were no browser long tasks or decoder resets. Bitrate was also lower, so this is not a controlled CPU-pressure experiment. The evidence supports keeping browser-host pressure separate from pre-proxy delivery stalls. It does not justify moving the entire video path to a worker yet.

## Review corrections

Parent review corrected the observer's handling of a WebSocket header that began while tracing was inactive. It previously assigned the later payload arrival as the first message byte. Such partial boundary messages are now skipped, with a regression test.

Independent review found that the analyzer could attribute a heartbeat delay whose deadline fell after the frame gap. The overlap calculation now uses the actual deadline and firing time, with boundary tests. A gap containing no observed heartbeat reports `null`, not a measured zero. A 100 ms heartbeat can miss shorter scheduling interruptions.

The analyzer now brackets each gap with TCP samples outside their collection-time uncertainty. Missing counters remain unknown. It compares adjacent-frame durations on each clock; it does not subtract native and browser clock epochs or claim physical input latency.

The strict probe TypeScript check and 24 focused proxy, framing, sampling, quality and correlation tests passed. Tests include real TLS forwarding and an owned live TCP socket. The full `pnpm check:rust` gate then passed: 82 Rust unit tests, 33 viewer tests and 37 recording/timing tests, plus the viewer build, Rust formatting and Clippy. ShellCheck and scoped Prettier checks passed. The six explicitly ignored Rust tests remain ignored here; the separate real-FFmpeg and IPC suites were not repeated in this diagnostics-only pass. A final native check verified health, process ownership and the unchanged installed/live hashes at 18:58:50 UTC. These local passes do not turn either live run into an acceptance pass.

## Next experiment

Keep the same binaries, viewer, workload and verified 8 Mbps quality. Compare the public HTTPS route with an owned `sprite proxy` TCP forward, then return to the public route. Keep the viewer on the same local acceptance URL and preserve its Origin rewrite. This can test whether the pauses depend on Sprite's public HTTP ingress path without replacing the product transport.

That bypass experiment has not been run. It also needs CPU observations in every run; a quiet browser host must not be mistaken for a better route. The CLI tunnel's localhost socket and its remote transport would need separate labels and ownership checks. Even a repeated improvement would show route sensitivity, not identify a particular internal proxy.

The deployed binary pair remains unchanged. Adaptive quality ended at 6400 kbps after these tests. The workload, browser, proxy and sampling tasks were cleaned up; the desktop service remains running.
