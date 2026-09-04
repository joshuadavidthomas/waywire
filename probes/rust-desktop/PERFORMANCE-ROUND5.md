# Public HTTPS versus a Sprite tunnel

Three 30-second recordings on 2026-09-06 compared public HTTPS, an owned Sprite CLI tunnel, then public HTTPS again. All three passed the existing performance and fidelity gates at 8 Mbps and full resolution. The tunnel showed no improvement. Delivery pauses remained on both routes.

This was a route comparison. Both routes carried WebSockets over TCP. It does not establish whether UDP media would perform better or whether TCP retransmissions caused the pauses.

## Conditions held constant

Each recording used the saved WLR pair, including the same embedded viewer:

| Binary  | Installed and live SHA-256                                         |
| ------- | ------------------------------------------------------------------ |
| Gateway | `0c771df4c0fb48d99971efc3133b52a7b3f0dd0474812bd8cd3ba72ddaa6abb4` |
| Streamd | `9d3260f9e2cca22a8aba464f5267602de5edf50080afc1fb76a94abbaef7c19f` |

Before every recording, the probe explicitly recreated only the owned `rust-desktop` service. The deployed API supports DELETE/PUT rather than the newer restart endpoint. Native identity checks confirmed that gateway, daemon and compositor PIDs changed while installed/live binary hashes stayed the same. No executable, viewer bundle or service configuration was uploaded. The worktree still builds the undeployed ext capture candidate.

Every arm started with fresh service and adaptive-controller state, a new isolated browser session, and a new FFplay workload. The workload requested 1824×848 at 60 Hz. Browser viewport, canvas dimensions, encoder flags and the 60 ms presentation target stayed unchanged. Warmup collected five valid moving-canvas observations. Actual encoder arguments showed 8000 kbps before recording, and every recorded SDK sample reported 8000 kbps at 100% scale. Adaptive quality remained enabled.

The tunnel experiment kept the browser at the same local acceptance URL. It forwarded HTTP and WebSockets through the CLI's loopback port 3218 to Sprite port 8080. The gateway still received its canonical public Host and Origin headers. The tunnel proxy never received a bearer token parameter and never forwarded browser cookies or authorization. CLI authentication stayed outside the browser. VNC, Waymote and other sessions' processes were untouched.

## Results

The directories below live under `results/`. Each contains the raw recording, native/gateway/browser timings, delivery and CPU samples, paired fidelity images, `summary.json`, and `delivery-analysis.json`.

| Directory                                | Presented FPS | Longest canvas freeze | Decoder resets | Paused RGB PSNR |
| ---------------------------------------- | ------------: | --------------------: | -------------: | --------------: |
| `performance-round5-public-before-ready` |     49.430532 |              149.8 ms |              0 |    35.532218 dB |
| `performance-round5-tunnel`              |     48.863735 |              150.0 ms |              0 |    35.550432 dB |
| `performance-round5-public-after`        |     49.930670 |              116.8 ms |              0 |    35.506713 dB |

All three retained one recording generation, zero decoder resets and a 33.4 ms p95 canvas-update gap. Their p95 confident lateness values were 15.53, 16.32 and 15.37 ms. Actual compressed payload rates were about 6.83, 6.82 and 6.85 Mbps; the 8 Mbps setting is an encoder target, not a promise of exactly 8 Mbps output.

The tunnel's slightly lower FPS does not establish that it is inherently slower. This is one short three-arm comparison without randomized order or a separate measurement of CLI CPU cost. It does show that this tunnel was not a fix in these runs.

## Pauses survived the route change

The analyzer matches generation and sequence across gateway writes, proxy receipts and browser callbacks. It compares adjacent-frame intervals on each clock, avoiding subtraction of unrelated clock epochs. Clock-rate skew remains uncalibrated.

| Measurement                                | Public before |    Tunnel | Public after |
| ------------------------------------------ | ------------: | --------: | -----------: |
| Matched frame intervals                    |          1528 |      1524 |         1532 |
| Largest gateway-write interval             |      45.89 ms |  38.94 ms |     40.72 ms |
| Largest complete-message interval at proxy |     147.34 ms | 162.46 ms |    118.76 ms |
| Proxy intervals over 100 ms                |             1 |         6 |            4 |
| Largest additional interval before proxy   |     128.41 ms | 144.95 ms |    100.06 ms |
| Largest additional interval after proxy    |      27.85 ms |  24.69 ms |     19.37 ms |

The largest values in each row need not describe the same frame pair. For one exact tunnel pair, generation 2 frames 1906 and 1907:

| Boundary                         |  Interval |
| -------------------------------- | --------: |
| Native capture                   |  16.10 ms |
| Gateway socket-write completions |  17.52 ms |
| Complete messages at local proxy | 162.46 ms |
| Browser receipt callbacks        | 162.60 ms |

The extra spacing was already present when the local proxy received the messages. There was no overlapping recorded browser long task. The Node heartbeat reported no positive delay for that gap; its 100 ms cadence cannot exclude every shorter scheduling interruption.

Another tunnel pair, frames 1398 and 1399, reached the proxy 131.76 ms apart despite gateway writes only 16.00 ms apart. The later message spanned 102.72 ms from its first byte to its last byte at the proxy.

These observations weaken the case for a problem unique to the public HTTP application route. They leave shared Sprite networking, TCP behavior, the intervening network and local receive processing under investigation. The CLI tunnel also introduces its own forwarding work. It is not a direct UDP path or a measured removal of every shared platform component.

## CPU and TCP observations

Maximum sampled host-wide CPU was 43.56%, 26.71% and 25.52%. No run recorded a browser long task. The earlier 90–97% host saturation did not recur. These roughly one-second samples do not prove identical scheduling conditions or exclude short stalls on individual threads.

Native capture sequence rates were 50.93, 50.88 and 51.10 FPS. They show the same roughly 51 FPS capture limit on both routes. They do not count every distinct source-application presentation. Median encoder CPU remained about one core, while FFplay consumed about 2.3 cores on the eight-CPU Sprite.

Each run collected 29 browser-host CPU samples. Public runs collected 102 and 103 upstream TCP samples. The tunnel run collected 103 Node-to-CLI loopback samples and 88 samples covering the CLI's two established remote TCP sockets. Observation errors and overflows were zero.

The Node socket and CLI sockets have separate labels. CLI sockets are listed in stable source-port order, without assigning one to video or control. Only numeric queues and counters enter the artifacts; no addresses or packet content are saved.

The remote TCP samples around the largest tunnel gap were about 703 ms apart. Their receive queues were empty at both endpoints. Retransmission and out-of-order fields were unreported. This is too sparse to exclude a short queue or establish packet loss. Local retransmission counters also do not measure the remote video sender's retransmissions.

## Failed preparation and review corrections

The earlier directory `performance-round5-public-before` records a failed preparation attempt. The service had started, but its health endpoint still returned 503 when the native check ran. No browser or workload launched. A subsequent native check found the service healthy with the saved hashes. The restart helper now waits up to 30 seconds for encoder health before running the identity checks. The failed attempt remains separate from the three completed recordings.

Parent review inspected the delegated HTTP/WebSocket forwarding and owned-child lifecycle code. Tests exercise real TLS forwarding, local HTTP/WebSocket forwarding, foreign-listener refusal, unsafe-listener cleanup, child exit, repeated close, and SIGTERM-to-SIGKILL escalation. A parent-added test samples the fixture child's actual TCP sockets and rejects sampling after that child exits.

Independent review found benchmark cleanup gaps. The probe now records browser cleanup responsibility before its first browser command. It records the FFplay PID before later identity validation can fail. Browser, proxy or tunnel cleanup failures fail the artifact, and proxy cleanup cannot skip cleanup of the workload and tunnel. Restart checks now prove process replacement as well as unchanged hashes. Every live comparison command explicitly included `--restart-trial`; an 8 Mbps snapshot alone is insufficient to establish fresh adaptive history.

The pre-live strict probe TypeScript check and 49 focused tests passed. Final validation also passed: `pnpm check:rust` (82 Rust unit tests, 33 viewer tests, 46 recording/timing tests, viewer build, Rust formatting and Clippy), strict probe TypeScript, three proxy tests, ShellCheck and scoped Prettier. Six Rust tests remained explicitly ignored in that command; the separate real-FFmpeg and IPC suites were not repeated for these probe-only changes.

The completed recordings confirmed browser, proxy and workload cleanup. The tunnel child was closed and reaped. A final listener check found neither local port 3217 nor 3218 in use. Native health and installed/live hashes passed again at 21:08:34 UTC. No transport change was deployed, and the service ended at 8 Mbps with the saved WLR pair.

## What to investigate next

A tunnel switch is not supported by this evidence. Before committing to a replacement media transport, collect bounded packet-header or kernel TCP evidence for the exact owned video connections. The missing observation is whether the pauses coincide with loss/retransmission, receiver-window pressure, or delayed forwarding while bytes are otherwise flowing. Prefer sender-side observations where permissions and connection ownership can be established; current receiver polling cannot answer that question.

Keep payloads and credentials out of that capture. A WebRTC feasibility check can separately establish UDP reachability, relay requirements and support for the current color fidelity. It should remain a small experiment until those constraints are known.

The severe half-second and one-second stalls from earlier rounds did not recur in these three recordings. Passing this short comparison does not erase them or establish long-run reliability.
