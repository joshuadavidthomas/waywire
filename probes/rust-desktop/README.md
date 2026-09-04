# Isolated Rust desktop trial

This directory defines a disposable `sprite-desktop-rust` trial. It does not
change the installed VNC desktop, the Waymote comparison, their recordings, or
the release installer. The trial has one Sprite service, `rust-desktop`:

1. `dbus-run-session` starts `run.sh`.
2. `run.sh` starts a private headless labwc/LXQt session and the Rust gateway.
3. The gateway starts the sibling Rust stream daemon.
4. The daemon starts FFmpeg and sends H.264 RTP back to the gateway over loopback.

The initial output is 1280×720 at 60 Hz. The encoder target is 8 Mbps. Breeze
cursors and the existing LXQt/labwc settings match the working Waymote trial.
Audio is absent: the Rust acceptance profile permits only `/stream` and
`/control` WebSockets.

The worktree currently builds the experimental persistent ext capture daemon.
The deployed trial uses WLR capture with the shorter keyframe interval and larger
FFmpeg pipe retained in [round six](./PERFORMANCE-ROUND6.md). Building and deploying
current source replaces that capture implementation. The earlier ext comparison
is in the [round-three results](./PERFORMANCE-ROUND3.md).
[Round four](./PERFORMANCE-ROUND4.md) separates pre-proxy stalls from browser-host
CPU pressure. It changes probes only; neither diagnostic run passed acceptance.
[Round five](./PERFORMANCE-ROUND5.md) compares public HTTPS, an owned Sprite tunnel
and public HTTPS again. All three recordings passed at 8 Mbps; the tunnel showed
no improvement. Round six kept a 15-frame keyframe interval and 1 MiB pipe after
live comparisons; recovery fell from about 1.16 s to 0.31 s in the controlled
exercise, and daemon CPU fell from 17% to 12% of one core. Its separate
`--exercise-recovery` phase leaves ordinary recording gates unchanged.
[Round seven](./PERFORMANCE-ROUND7.md) repeated WLR/ext capture with visible frame
IDs. Persistent capture reached 55.06 distinct drawn FPS, but each arm passed once
and failed once. The exact round-six WLR pair remains deployed.
[Round eight](./PERFORMANCE-ROUND8.md) paused the host-load comparison after the
first WLR recording failed and the host did not stay lightly loaded. No binaries
changed; the remaining three arms await a controlled host window.

A separate [native-video prototype](../webrtc-exploration/README.md) now plays
full-chroma VP9 through WebRTC and a normal browser video element. It is local-only
and does not replace this deployment. Its text chart passed the existing fidelity
threshold; Sprite connectivity and interactive acceptance remain untested.

## Safety rules

Only the exact Sprite name `sprite-desktop-rust` is accepted. The TypeScript
probes reject `josh-desktop`, `sprite-desktop-v1`,
`sprite-desktop-v1-conflicts`, and `sprite-desktop-waymote`. They also require
`auth: sprite` and `private_access: admins` as pre-existing settings. They never
create a Sprite or change URL policy.

`prereq-check.ts` is read-only. It reports missing commands or packages and
stops; it does not run apt or provision the host. `deploy.ts` is the only
installation probe. It requires the literal `--authorize-deploy` flag, refuses a
protected or foreign HTTP service before writing a file, and requires its own
marker before replacing an existing trial tree. It uploads two binaries supplied
by path and records their SHA-256 values with the current jj revision. Deployment
still needs a separate operator authorization; the flag records that decision
rather than granting it. Live input probes change the test application;
`capture-native.ts` creates and removes a temporary screenshot;
`ffmpeg-restart-check.ts` kills one validated FFmpeg child. Run these only on
the authorized trial.

`run.sh` accepts only the `rust-desktop` service cgroup. It uses
`/tmp/sprite-desktop-rust-session`, refuses a foreign active Wayland socket or
port 8080 listener, removes only an inactive socket inside its owned private
runtime directory, and kills only the labwc and gateway PIDs it launched. A
socket counts as ready only while that labwc PID is alive and `wayland-info` can
connect to it. The launcher never searches for or kills a pre-existing
compositor.

## Host prerequisites

The disposable Sprite must already contain the packages used by the known
Waymote labwc/LXQt setup: `labwc`, `lxqt-core`, `lxqt-wayland-session`,
`breeze-cursor-theme`, `ffmpeg`, `dbus-x11`, `wayland-utils`, and `wlr-randr`.
It also needs `jq`, `flock`, `ss`, and `sprite-env`, plus kernel permission and
pipe quota for a 1 MiB FFmpeg stdin pipe. Check packages without changing the
host:

```sh
pnpm exec tsx probes/rust-desktop/prereq-check.ts \
  --sprite sprite-desktop-rust
```

Build the viewer and both binaries locally. After separate deployment approval,
run the mutating command with explicit binary paths:

```sh
pnpm --filter @sprite-desktop/stream-viewer build
cargo build --release --workspace
pnpm exec tsx probes/rust-desktop/deploy.ts \
  --sprite sprite-desktop-rust \
  --gateway target/release/sprite-desktop-gateway \
  --streamd target/release/sprite-desktop-streamd \
  --authorize-deploy
```

The checked-in `service.json` is the wire-shaped copy of the service definition
used by `deploy.ts`. Do not register it on a protected or shared Sprite.

## Checks

Local checks need no Sprite and may be run before any authorization:

```sh
pnpm exec tsx --test probes/view-runtime.test.ts
pnpm exec tsc -p probes/tsconfig.json
shellcheck probes/rust-desktop/run.sh
```

The proxy test creates a temporary loopback HTTPS certificate with the real
`openssl` command, trusts it through one `https.Agent`, and deletes the key and
certificate in `finally`. It does not disable TLS verification process-wide.
It checks the fixed Rust HTTP/WebSocket routes, browser authority and Origin,
token forwarding, request and response cookie stripping, and credential
revocation while the proxy itself listens on an ephemeral port with an explicit
`Host: 127.0.0.1:3217` header. The active VNC and Waymote proxy profiles remain
unchanged.

After an authorized deployment, run the live video checks:

```sh
pnpm exec tsx probes/rust-desktop/native-check.ts \
  --sprite sprite-desktop-rust --suite video
pnpm exec tsx probes/rust-desktop/smoke.ts \
  --sprite sprite-desktop-rust --suite video
```

`native-check.ts` checks the owned service definition, local health, distinct
Rust gateway/daemon PIDs, FFmpeg parentage, the gateway listener, the daemon's
private Wayland environment, and hashes of both installed and live `/proc/PID/exe`
binaries against the deployment marker. `smoke.ts` uses
the token-isolating proxy and agent-browser to check a real first frame, two
viewers, non-flat pixels, and a static-desktop reconnect. The browser receives no
Sprite token.

## Evidence status

On 2026-09-05 Josh directed verification on a Sprite. The disposable trial was
created, its desktop prerequisites installed, and a matched release build of both
Rust binaries deployed. Its URL is
https://sprite-desktop-rust-6ra.sprites.app/ with `auth: sprite` and
`private_access: admins`. Existing Waymote and VNC desktops were not changed.

Passed against the real Rust pair:

- Prerequisites and native topology: separate gateway, streamd and FFmpeg PIDs,
  loopback RTP, port 8080, binary hashes and health.
- First video frame, two viewers and static-desktop reconnect at 1280×720.
- A 1600×1000 browser viewport changed the native output to 1552×832 at 60 Hz.
- Browser key input appeared in FeatherPad. Clicking its Save button produced a
  real file. Separate Shift and Control key events produced uppercase `R` and
  saved it with Ctrl+S. See `results/typing-before.png` and `typing-after.png`.
- Browser dragging moved FeatherPad from roughly 425,132 to 564,220. Browser
  wheel input scrolled its generated test document from lines 136–160 to
  112–136. See `results/drag-after.png` and `scroll-{before,after}.png`.
- Native clipboard text, including Ω, reached the browser clipboard. Browser
  clipboard text reached `wl-paste`. Only generated test text was used.
- Arrow, I-beam and resize cursor images appeared. The I-beam used hotspot
  16,15 without pointer lock. Explicit pointer lock and Escape exit worked;
  a custom cursor returned after reacquisition.
- Killing validated FFmpeg PID 4612 produced replacement PID 4658, changing
  SSRC 5 to 6 while gateway 4198 and streamd 4207 stayed alive. Typing `x`
  afterward appeared in both the native screenshot and the existing browser:
  `results/restart-native.png` and `restart-browser.png`.

The restart probe checks process replacement and SSRC, not browser presentation
or wire generation. The screenshots provide the separate presentation check.
A cached FPS label cannot prove recovery; an initial version of the probe used
that weak assertion and it was removed during parent review.

Local checks passed on the same date: 40 Rust unit tests, all three explicitly
selected real-FFmpeg tests, 17 stream-viewer tests, strict TypeScript, Clippy,
Rust formatting, hostile-peer IPC/HTTP checks, the loopback proxy test,
ShellCheck and release builds. Shared regressions also passed: seven VNC viewer
tests, its check/build, and three recorder-metrics tests. Dependency and runtime
link inspection kept Wayland and XKB out of the gateway.

Still unverified against the actual compositor:

- Abrupt control-socket loss while keys or buttons are held.
- Malformed, truncated and EOF stdin while native input is held.
- Resize bursts paired with rejection of stale keyframe readiness.
- Daemon death, whole-service recovery and orphan-child checks.
- A link cursor and application-requested cursor hiding.
- User comparison and physical input latency. No timing label establishes either.

Component and hostile-peer tests cover parts of these cases, but do not replace
the paired checks. The seven-slice outline is not fully accepted. VNC release,
migration and OS privilege separation remain outside this trial.

## Continuous-motion performance gate

`performance.ts` runs a fixed real workload rather than a synthetic browser
animation. It starts one owned `ffplay` process inside the private Wayland
session. FFplay renders `testsrc2` at the native 1824×848, 60 Hz mode in a
fullscreen window. The browser receives the desktop through the Rust service at
a requested 1867×986 viewport; the measured recording dimensions must remain
1824×848. The probe neither deploys nor restarts the service.

The gate records 30 seconds through the optional comparison recorder, saves its
full JSON, samples whole-Sprite and per-process CPU through `/proc`, and captures
native and browser PNGs before and after recording. Native luminance range,
browser canvas color samples, and changed image hashes show that the evidence
contains changing test-source pixels. The fixed limits are at least 45 canvas
update FPS, at most 50 ms p95 frame gap, at most 100 ms clock-confident p95
lateness, and at most 4 frames at p95 decoder queue depth. The longest freeze,
including both recording edges, must stay below 250 ms; decoder resets fail the
gate. Missing clock confidence fails too. Queue samples come from draw-triggered
SDK stats, so reset instrumentation supplies a separate failure signal.
`--fidelity` sends a real `p` key through the browser after timing. It requires
at least 35 dB RGB PSNR between native and decoded canvas PNGs, with two identical
native captures bracketing the canvas capture. This measures a paused chart,
not dynamic frame quality or physical input latency.

Each output path must be new. The probe accepts no remote command or arbitrary
target:

```sh
pnpm exec tsx probes/rust-desktop/performance.ts \
  --sprite sprite-desktop-rust \
  --output probes/rust-desktop/results/performance-$(date -u +%Y%m%dT%H%M%SZ) \
  --fidelity
```

The corrected baseline delivered 4.80 FPS with 240.5 ms p95 reported lateness.
The first corrections reached about 50 FPS. The follow-up critical-path changes
still deliver 49–51 FPS; the final run measured 50.96 FPS, 15.93 ms p95 lateness,
166.6 ms longest freeze, no decoder resets and 35.67 dB paused-chart RGB PSNR.
It includes live binary identities, recording-edge freezes and a zero-reset gate.
See [results, matched capture experiment and limits](./PERFORMANCE.md). The
service still targets 60 FPS; these measurements do not establish sustained
60 FPS or a steady-state throughput gain from the follow-up changes.

Add `--stages` for bounded native and gateway timings. The launcher provides
owned, unique paths; each process creates a private file and records nothing
until the probe starts it. The probe validates both processes before sending
SIGUSR1/SIGUSR2. It saves native and gateway raw traces and summaries, requires
zero overflow and at least 1000 correlated spans, and rejects stale snapshots.
Native storage holds at most 32,768 records; gateway storage holds 65,536. Both
skip clocks, record construction and mutexes when inactive. The traces contain
identifiers and timestamps, never pixels, H.264 payload, input or clipboard.
The launcher removes their directories on shutdown.

The browser observer records header, decoder and long-task timestamps, then
joins decoder output to canvas draws by media timestamp. To inspect the largest
receipt gaps against completed gateway writes in a traced result directory:

```sh
pnpm exec tsx probes/rust-desktop/analyze-delivery.ts \
  probes/rust-desktop/results/performance-round3-ext-confirm
```

Socket completion means local acceptance, not browser delivery. The script
compares durations on each clock without subtracting their epochs.

Add `--delivery` alongside `--stages` to record proxy message arrivals, owned
browser/driver CPU, local Node heartbeat delays and the proxy's upstream TCP
counters in `delivery.json`. This requires local Linux `/proc`, `ps`, `ss` and
`getconf`. It reads only numeric TCP fields and owned process/thread stats, never
packet contents, other sessions' command lines or credentials. Analyze that
additional evidence with:

```sh
pnpm exec tsx probes/rust-desktop/analyze-delivery.ts \
  probes/rust-desktop/results/performance-round4-wlr-tcp --proxy
```

Fixed-quality recording now requires an actual 8000 kbps encoder before it starts
and 8000 kbps at 100% scale in every SDK sample. A prior adaptive reduction must
be resolved before making that comparison. By default, the probe leaves the
service and its adaptive state alone.

### Comparing delivery routes

`--route tunnel` starts and owns a Sprite CLI forward from loopback port 3218 to
remote port 8080. The browser still uses the acceptance proxy on port 3217. The
public route remains the default; its canonical HTTPS and credential checks are
unchanged. The tunnel mode preserves gateway Host/Origin validation without
forwarding browser credentials.

`--restart-trial` explicitly recreates only the exact owned `rust-desktop` service
before measuring. This restarts the desktop and its applications. It changes no
installed files. Use it for every arm of a route comparison, even if the last
arm ended at 8 Mbps: adaptive history must also start fresh. The probe verifies
new process PIDs and unchanged installed/live hashes. Do not run another deploy
or service-control task against this trial during the comparison.

Choose a new experiment directory; existing results are never overwritten:

```sh
experiment="probes/rust-desktop/results/routes-$(date -u +%Y%m%dT%H%M%SZ)"
pnpm exec tsx probes/rust-desktop/performance.ts --sprite sprite-desktop-rust \
  --output "$experiment/public-before" --route public --restart-trial --fidelity --stages --delivery
pnpm exec tsx probes/rust-desktop/performance.ts --sprite sprite-desktop-rust \
  --output "$experiment/tunnel" --route tunnel --restart-trial --fidelity --stages --delivery
pnpm exec tsx probes/rust-desktop/performance.ts --sprite sprite-desktop-rust \
  --output "$experiment/public-after" --route public --restart-trial --fidelity --stages --delivery
```

Inspect each failed run and confirm cleanup before starting the next arm. Compare
hashes, viewport, warmup, recorded quality and host CPU as well as FPS. With both
`--stages` and `--delivery`, the probe saves `delivery-analysis.json` automatically.
Tunnel recordings label Node-to-CLI loopback TCP separately from the CLI's remote
TCP sockets. Neither route tests UDP.

The installed Services API requires DELETE followed by PUT. If recreation fails
after deletion, stop the comparison. Restore only the definition in `trial.ts`
through the Services API after rechecking ownership; do not rebuild or deploy the
worktree, which currently contains the ext capture candidate.

Viewer diagnostics now use monotonic received, decoded, presented and per-cause
drop counts. The summary reports first-to-last sample deltas, which exclude the
small gaps between those samples and the exact recording edges. The human footer
updates at 4 Hz. Ordinary viewing also requests SDK snapshots at that cadence;
recording and the SDK default retain a snapshot for each presented frame.

The trial now uses H.264 High 4:4:4 Predictive (`avc1.F40034`), full-range BT.709
with sRGB transfer metadata. Only Chrome 152 on Linux was verified. Hardware
decoding and other browsers may not support this profile; there is no baseline
fallback. The SDK reports an unsupported codec when its capability check fails.

Each run terminates only its validated FFplay PID and closes its own browser and
proxy. It leaves the desktop service running. The final settings retain an 8 Mbps
encoder target and 60 ms presentation target. Current source checks cover 82 Rust
unit tests, four explicit real-FFmpeg tests (including the emitted SPS color/profile
contract), 33 viewer tests and 46 recording/timing tests. IPC regressions also
exercise blocked output while input continues to reach the native peer, followed
by lease handoff. These checks do not replace held-input failure acceptance
against the compositor.

## Browser driver notes

Use `agent-browser` with a named, trial-only browser. Its current typing helper
omits `KeyboardEvent.code`, and its shortcut helper does not send a separate
modifier keydown. Single-key presses work; `browser-input.ts` supplies complete
CDP key and held-button drag sequences when needed. Verify their result in the
native application rather than accepting the driver's success message.

The current `--allowed-domains` browser wrapper strips WebSocket's static state
constants and breaks the SDK's state checks. This probe uses only its fixed local
proxy, omits that wrapper and validates the native constants before recording.

The CDP helper accepts a loopback debugging port for that private browser only.
Its `permissions` action holds clipboard permissions open until SIGTERM or
SIGINT; closing the DevTools connection removes the override. Keep its PID and
stop it after testing. Do not grant permissions in a user's regular browser.
