---
type: structure-outline
repo: sprite-desktop
change: nkspolspoyvu
revision_at_reconnaissance: 232412ffa165b51cf586f4dfbc06fb7cc256148d
date: 2026-09-05
status: in-progress
source_design_discussion: 001-design-discussion.md
---

# Rust desktop server: two-process implementation outline

## Review status

Josh authorized implementation from this revised [two-process design](001-design-discussion.md), then directed verification on a Sprite. Both executables and the viewer now run on the private `sprite-desktop-rust` trial. Local tests and basic desktop checks pass; combined failure acceptance remains incomplete. No separate executor plan was written. See the trial README for evidence; the gates below are requirements, not a claim that every case passed.

The process count, ownership split, retained IPC, loopback RTP, attribution and deferred hardening are settled. This outline does not introduce a desktop library linked into the gateway.

## End state and dependency boundaries

Two private Cargo packages produce two executables:

- `crates/streamd/`, package/binary `sprite-desktop-streamd`: Wayland capture, virtual input, text composition, clipboard, output management, cursor capture, raw-frame buffering and FFmpeg supervision.
- `crates/gateway/`, package/binary `sprite-desktop-gateway`: browser HTTP/WebSockets, embedded viewer, controller/quality policy, daemon supervision, RTP reconstruction, metadata correlation, keyframe bootstrap and viewer queues.

Neither package depends on the other's implementation. The gateway launches the daemon with command/event pipes. The daemon launches FFmpeg; FFmpeg sends H.264 RTP to a loopback socket owned by the gateway. Native handles and mapped buffers never cross into gateway memory.

The owned TypeScript/Vite viewer, with Effect managing session and permission tasks, lives in `apps/stream-viewer/`. Preserve the 60 FPS/8 Mbps startup targets, encoder flags, adaptive quality, browser 60 ms latency selection, responsive sizing and current cursor/control behavior. Audio, laptop-native cursor roles and engagement changes remain outside the first port.

Separate address spaces are part of this port. Different service identities, sandbox policies and enforced gateway denial of Wayland/filesystem access are not. No test or documentation may call the port sandboxed merely because it has two processes.

## Source anchors

Start from upstream commit `90564cfb02030c494939c6fdf29cae9c4d689c67` and `probes/compare/waymote.patch`. The inspected `/tmp/waymote-comparison-source` tree is a reference, not a permanent build prerequisite.

| Anchor                                                                                                 | Use                                                                                                                                     |
| ------------------------------------------------------------------------------------------------------ | --------------------------------------------------------------------------------------------------------------------------------------- |
| `probes/waymote/README.md`, `probes/compare/README.md`                                                 | Existing reconstruction recipes, recorder/metrics copy step and experiment constraints. Record the pinned source archive checksum here. |
| Reference `gateway/sdk/{waymote.js,audio-player.js,waymote.d.ts,waymote.test.mjs,cursor.test.mjs}`     | Owned browser import, relative AudioWorklet asset and retained tests.                                                                   |
| Reference `gateway/examples/web/{index.html,client.js,style.css}`                                      | Copy the working viewer before adapting imports/build paths.                                                                            |
| Reference `gateway/main.go`, `validControlRecord`, control handler, video encoding                     | Actual browser message validation and 40-byte header.                                                                                   |
| Reference `gateway/main.go`, `runEncoder`, daemon event parser, RTP receiver and video hub             | Spawn/pipe ownership, metadata correlation, keyframe-readiness writes and viewer recovery.                                              |
| Reference `main.zig`, command parser, `submitFrame`, `advanceVideoGeneration`, `waitForDamage`         | Native record semantics, timestamps, generation changes and the gateway/native readiness handshake.                                     |
| Reference `VideoEncoder.zig`, `submit`, `runInner`, `writeFrame`                                       | Raw-frame pool, partial writes, metadata publication and FFmpeg restart coordination.                                                   |
| Reference `gateway/main_test.go`, especially `TestRTPAssemblerEmitsAccessUnitAtMarkerWithoutNextFrame` | Packet, timing and idle-final-frame fixtures.                                                                                           |
| Patched `CursorCapture.zig`, `gateway/cursor.go`, cursor tests; `probes/compare/CURSORS.md`            | Cursor event framing, alpha conversion, cache/visibility and evidence.                                                                  |
| `probes/view-runtime.ts`, `createAcceptanceProxy`                                                      | Credential-isolating proxy, actual route allowlists and HTTP/WS cookie stripping.                                                       |
| `probes/compare/smoke.ts`; `bridge/server.go`, `bridge/server_test.go`                                 | Browser/proxy cleanup patterns and canonical Origin rejection tests.                                                                    |
| Root `package.json`, `pnpm-workspace.yaml`; `apps/viewer/package.json`                                 | Existing scripts, Node/pnpm requirements, `apps/*` membership and Vite 8.2.2.                                                           |

At reconnaissance, the repo has no root LICENSE file. Add the applicable upstream notice and explain its scope in README; preserve core and SDK notices without inventing a licensing decision for unrelated code. The existing proxy strips cookies but has no direct tests of that behavior or its route allowlists. Slice 3 adds those tests.

## Three contracts to preserve

| Boundary                                                          | Retained contract                                                                                                                                               | Owner of validation                                      |
| ----------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------- |
| Browser to gateway                                                | 16-byte binary control types 1–6 and 8; literal `acquire`/`release`; supported ping, feedback, composition and clipboard JSON                                   | Gateway browser parser                                   |
| Gateway stdin commands to daemon, daemon stdout events to gateway | Patched version-2 command headers and variable text/clipboard payloads; 8-byte event headers with clipboard, frame metadata, resize-applied and cursor payloads | Each receiving process independently validates its input |
| FFmpeg to gateway, gateway to browser                             | Loopback H.264 RTP; one Annex-B access unit after the browser's 40-byte video header                                                                            | Gateway RTP/correlation and browser serialization        |

Daemon command types for clipboard, quality, composition and keyframe readiness are not automatically valid browser binary commands. Keep the browser's timestamp meanings and the string representation of `serverNanos` in pong JSON. Audio events/routes and raw H.264 on daemon stdout are outside this port; daemon stdout is for events, stderr for diagnostics.

Within each process, translate bytes into small private types. Across processes, retain serialization. No shared runtime library, shared-types crate, feature-selected backend, alternate wire shape or public test-only API is needed.

Use golden fixtures checked against the reference rather than relying solely on Rust encoder/decoder round trips. Put shared wire fixtures under `probes/rust-desktop/fixtures/` with attribution and expected outcomes; both packages may read them in tests without acquiring a runtime dependency on each other.

## Work sequence

| Slice                                          | Observable result                                                                                                       | Depends on           |
| ---------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------- | -------------------- |
| 1. Own the browser and pin the contracts       | Both packages and browser build/tests work from repo inputs; wire fixtures establish the reference contract.            | Accepted design      |
| 2. Implement media and process plumbing        | Native FFmpeg writes and gateway RTP/event handling work; real pipes expose framing, backpressure and cleanup failures. | 1                    |
| 3. Show the real desktop through both binaries | The retained browser decodes actual labwc frames through the Rust daemon and gateway, including the keyframe handshake. | 2                    |
| 4. Control the desktop across ordered IPC      | One controller can use pointer, keyboard and composition without stale-owner input or stuck releases.                   | 3                    |
| 5. Resize and exchange clipboard text          | Requests, native events, generation changes and text transfers complete through both processes.                         | 4                    |
| 6. Preserve cursor behavior                    | Native cursor events become the existing browser image/hotspot/visibility state.                                        | 4; integrate after 5 |
| 7. Verify combined failure and comparison      | Process death, blocked pipes, viewer overload and reconnect produce bounded recovery or honest session failure.         | 1–6                  |

Slice 2 tests components and real IPC before introducing Wayland scheduling. Slice 3 supplies the first full paired-binary video gate. Test peers in slice 2 are fixtures, not an alternative daemon shipped to users, and do not establish native parity.

## Shared gates and test placement

The paths and new commands below are implementation outputs. They do not exist or pass merely because this outline names them.

From slice 1 onward, each slice runs applicable package tests and:

```sh
pnpm --filter @sprite-desktop/stream-viewer build
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Build viewer assets before compiling the gateway. `cargo test -p sprite-desktop-streamd` must not need those assets. Gateway-only builds/tests must not link Wayland libraries or compile the daemon package. Neither package's ordinary unit tests should connect to the user's compositor or launch the real peer unexpectedly.

Keep pure tests beside private modules. Private network helpers may consume real readiness channels and frame streams for local tests. Process tests exercise the binaries through their real pipes, CLI and HTTP/WS endpoints. A private spawn seam can accept a dependency-native process command; do not build a generic backend trait or an embedded fake desktop to obtain test access.

Explicit native groups cover FFmpeg and live Wayland. Report prerequisites, selected tests and results. Missing prerequisites, zero selected cases or skipped required cases do not pass the corresponding gate. Test fixtures may emulate pipe peers to exercise hostile input; the final paired gate must use both actual Rust executables.

Touched TypeScript probes run `pnpm exec tsc -p probes/tsconfig.json`. Launchers run ShellCheck. Recorder changes run `node --test probes/compare/metrics.test.mjs`. Format only changed files. No passing local test is evidence of an OS sandbox or actual browser-to-app input delivery.

## Slice 1: own the browser and pin the contracts

### Changes

- `README.md`, new `LICENSE`: explain the derivative work and preserve upstream notices. Keep existing VNC instructions.
- `probes/waymote/README.md`, `probes/compare/README.md`: record immutable source reconstruction inputs and the separate recorder/metrics step. No `third_party/`, provenance package or new patch series.
- `apps/stream-viewer/package.json`, `index.html`, `vite.config.js`, `src/`: copy the patched example, SDK, declarations, relative `audio-player.js` and tests before making targeted import/build changes.
- Instantiate `audio: false`, remove audio UI, retain responsive layout and explicit-only pointer lock. Verify Vite resolves the worklet asset while normal use loads no audio socket/worklet. Import the existing recorder only for recording mode.
- Root `Cargo.toml`, `Cargo.lock`, `rust-toolchain.toml`: two independent binary packages, explicit resolver, pinned supported toolchain, shared dependency versions and `publish = false`.
- `crates/streamd/{Cargo.toml,src/main.rs,src/protocol.rs}`: CLI help/version and the bounded daemon command parser/event encoder, tested without Wayland execution.
- `crates/gateway/{Cargo.toml,src/main.rs,src/protocol.rs,src/daemon.rs}`: CLI help/version, embedded assets, browser wire conversion and the opposite IPC encoders/decoders. Add only the real behavior needed now; native startup arrives later.
- `probes/rust-desktop/fixtures/`: attributed golden commands, events and browser headers, with expected decoded values and invalid cases. Select cases from the patched reference, including cursor events, rather than copying just the older protocol document.
- Root `package.json`, `.gitignore`, `.prettierignore`: narrow Rust build/check orchestration and ignored `target/`. Build assets before gateway embedding; Cargo build scripts do not run pnpm. Do not alter the existing release/install commands.

Parsing contract: both sides handle split headers/payloads and multiple records per read. Validate version, type, reserved fields, lengths and semantic bounds before allocating or performing native work. Unknown/truncated records fail explicitly; do not scan forward hoping to recover framing.

### Validation

- `pnpm --filter @sprite-desktop/stream-viewer test` and `build`.
- `cargo test -p sprite-desktop-streamd` before any viewer build; `cargo test -p sprite-desktop-gateway` after asset generation.
- Golden tests cover exact commands/events, browser headers, invalid UTF-8, non-finite pointer values, oversized allocations, fragmented records and clean versus truncated EOF. A value rejected at the browser boundary must not gain access to a daemon-only opcode.
- `cargo run -p sprite-desktop-streamd -- --version` and the equivalent gateway command work without a desktop and identify both builds. Neither binary fakes a successfully running desktop.
- Verify the Cargo dependency graph keeps implementation and Wayland dependencies out of the gateway. Reconstruct the pinned reference and dry-run its saved patch; rebuilding old binaries stays opt-in.

## Slice 2: implement media and process plumbing

### Daemon work

- `crates/streamd/src/video.rs` and substantial private submodules: preserve the three raw-frame slots, latest-pending replacement, FFmpeg stdin writing, cancellation and child supervision.
- Complete an entire raw-frame write before publishing its metadata event. A discarded pending capture gets no metadata. An unrecoverable partial write invalidates the child; never append a new raw frame after abandoning part of the previous one.
- Use the gateway-provided loopback RTP destination. The daemon owns the FFmpeg child; it does not own a gateway-style RTP receiver or subscriber hub.
- Add one stdout event writer separate from diagnostic stderr. Ordered metadata/resize events cannot use the replaceable cursor-state policy. Bound queued counts and bytes, and fail safely when required events cannot progress.
- Test native media internals with distinguishable raw-frame fixtures. This must not add a synthetic-desktop mode to the production daemon.

### Gateway work

- `crates/gateway/src/main.rs`, `daemon.rs`: wire the executable's run path to the child supervisor and media pipeline now, so the IPC checker exercises the actual gateway rather than just a private parser. HTTP/browser service is added in slice 3. Bind RTP before starting the configured daemon; own stdin/stdout, child identity and joined cleanup. The configured executable path is trusted launch configuration, never browser input.
- Serialize commands through one writer. Read events independently so a blocked stdin write does not stop stdout draining. Implement bounded failure for blocked/broken pipes and malformed peer output.
- `crates/gateway/src/video.rs`, `video/{rtp,hub}.rs` and cohesive correlation state: reconstruct access units, handle packet sequence/timestamp wrap and loss, correlate metadata, maintain bounded GOP bootstrap and recover slow subscribers at a keyframe.
- Preserve the generation/keyframe-readiness messages sent back to the daemon. Generation transitions clear stale correlation and cached decoder state. Neither side may invent metadata pairings after overflow or loss.
- Add `probes/rust-desktop/ipc-check.ts` and a test-only peer under `fixtures/`. The peer uses real OS pipes, can split/misframe/withhold output, and records command order. It must not import the gateway parser it is testing. Keep it out of the shipped pair and normal startup path.

### Validation

- Package unit tests cover pool replacement, packet fragmentation/loss/wrap, metadata/RTP arrival order, generation changes, byte budgets and viewer overflow.
- Establish `cargo test -p sprite-desktop-streamd ffmpeg_ -- --ignored` as an explicit real-FFmpeg test group. Keep the expected cases and required FFmpeg capabilities documented. Because this is a binary package, do not assume a `--lib` target exists.
- Submit distinguishable frames through actual FFmpeg, then inspect/decode the resulting access units and check associated metadata. Hold stdin open after a final changed frame: that frame must arrive without a later frame or EOF.
- After viewer asset generation, run `cargo build -p sprite-desktop-gateway` to produce the executable; `cargo test` alone does not guarantee the unhashed binary exists. Then run `pnpm exec tsx probes/rust-desktop/ipc-check.ts --gateway target/debug/sprite-desktop-gateway` to exercise the actual gateway's pipes against the controlled peer. Cover concatenated/split events, event floods, blocked reads, malformed/truncated EOF, readiness commands and child exit. The checker sets up the explicit peer path; it is not an automatic fallback.
- Test both directions under backpressure together. The result must be continued bounded progress or bounded session failure, never a write/read deadlock or silent metadata loss.
- Test FFmpeg death/restart here, including discarded old metadata, new-generation readiness and child reaping. Include one complete trace that forces a generation-changing restart during a partial raw-frame write: no metadata for the incomplete frame, old child invalidated/reaped, old readiness rejected, and the next frame submitted only to the replacement child under the new generation.
- Test gateway supervision with a controlled child that exits or stops reading. Slice 7 repeats these transitions in the complete system.

These checks establish native media components and gateway IPC behavior separately. They do not claim the real daemon and gateway have completed a live Wayland handshake until slice 3.

## Slice 3: show the real desktop through both binaries

### Changes

- `crates/streamd/src/main.rs`, `wayland.rs`, `wayland/capture.rs`: connect to the configured compositor, discover required globals/output, and own SHM and dispatch on the native thread. Copy completed captures into encoder slots before compositor reuse.
- Connect the daemon command loop, event writer and media worker. Capture-ready timestamps use host `CLOCK_MONOTONIC`; gateway pongs use the same domain.
- Complete the generation/keyframe handshake through actual pipes. A gateway-ready command must refer to the current generation. Native damage-driven idle begins only after the current generation has a usable keyframe; delayed old-generation readiness cannot stop capture.
- `crates/gateway/src/main.rs`, `http.rs`, `daemon.rs`, `protocol.rs`: serve the embedded viewer, `/stream`, `/control` and maintained `/healthz` state. Serialize correlated video to the retained browser header. Give each WebSocket one writer and joined cancellation.
- During this view-only milestone the retained viewer may attempt acquire, resize or feedback. Keep its socket/pong flow alive using the existing non-active control behavior; ignore valid controller-only work while no controller is active. Do not report input active, acknowledge unapplied resize or introduce a permanent test/view-only protocol. Slice 4 supplies real acquisition.
- Enforce exact canonical Origin before upgrade, fixed routes, bounded messages and appropriate asset/cache/security headers before exposing the trial. No `/audio`, `/vnc`, daemon-IPC HTTP route or generic proxy.
- `probes/view-runtime.ts`, new `probes/view-runtime.test.ts`: add a named Rust allowlist for Vite assets and `/stream`/`/control`; preserve actively used VNC/Waymote profiles. Test HTTP/WS cookie stripping, authority/route rejection, credential forwarding and revocation with loopback fixtures and scoped TLS trust.
- `probes/rust-desktop/{README.md,run.sh,service.json,native-check.ts,smoke.ts}`: define the disposable trial. Copy the known launcher first, then configure the explicit Rust gateway and daemon paths. The launcher starts labwc/LXQt and the gateway; the gateway starts the daemon; the daemon starts FFmpeg.
- Build/install both binaries from the same source revision with recorded identities. Keep the known Breeze/LXQt environment and initial 1280×720@60 Hz mode. Check compositor readiness against the process actually launched, not a stale socket. Preserve environment keyboard-layout settings.

### Validation

- Gateway tests exercise HTTP, readiness states and socket cancellation with private channel/stream seams, without a live Wayland dependency. Native integration proves those states reflect actual process health.
- `pnpm exec tsx --test probes/view-runtime.test.ts`, TypeScript probe checks and `shellcheck probes/rust-desktop/run.sh`.
- After deployment authorization, run `pnpm exec tsx probes/rust-desktop/native-check.ts --sprite sprite-desktop-rust --suite video` and the corresponding `smoke.ts --sprite sprite-desktop-rust --suite video`.
- Confirm the runtime topology: distinct gateway and daemon PIDs, FFmpeg owned by the daemon, browser listener in the gateway, loopback RTP receiver in the gateway, and Wayland access in the daemon. This is topology evidence, not proof that gateway socket access is denied by the OS.
- Render actual labwc pixels through both Rust binaries. Verify shared clock behavior, initial keyframe, final idle frame, two viewers, static-desktop reconnect and stale keyframe-readiness rejection.
- `/healthz` must become unavailable or the gateway must exit on fatal daemon/pipe failure. Ordinary idle capture stays healthy without health polls creating frames. Graceful shutdown must reap owned children.

Both actual Rust processes are required for this gate. A fixture peer or a mixed-language diagnostic run is not a passing paired-binary result.

## Slice 4: control the desktop across ordered IPC

### Changes

- `crates/streamd/src/wayland/input.rs`: virtual pointer/keyboard, keymap/modifiers, repeat, buttons/scroll, absolute/relative motion, composition commit/preedit and release-all. Parse and validate daemon records independently before applying them.
- `crates/gateway/src/session.rs`, `protocol.rs`, `daemon.rs`: acquire/busy/active/release behavior and browser-to-daemon conversion. Keep controller identity local to the gateway; do not add an epoch field to the retained wire format.
- Recheck ownership at the ordered command writer, not only when a browser message arrives. Purge/reject stale queued controller work. A partially written record must finish or fail the session before another record can be written.
- Ensure release-all is ordered before every new owner's input. The daemon processes that order. `active` means controller ownership; pipe completion does not acknowledge compositor application. The retained protocol has no release-applied event, so do not recreate the superseded awaited library API.
- Saturation must not silently lose release-all/key-up or grant a new owner without an ordered release boundary. Use a bounded unhealthy-session shutdown if progress is impossible.
- On stdin EOF, fatal command framing/validation failure and orderly shutdown, release held native input and close resources. Keep FFmpeg cleanup independent of successful final stdout delivery.
- Populate capture metadata with the latest actually applied input sequence. Do not advance it just because the gateway accepted or queued a browser record.

### Validation

- Unit and real-pipe tests cover two contenders, repeated acquire/release, delayed old-owner work, partial writes during takeover, queue saturation and shutdown during release. Assert actual command order observed by the peer, not just calls to a mock writer.
- Extend native/browser probes with `--suite input`. Check normal controller disconnect and gateway death while keys/buttons are held. Also drive the actual daemon over test-owned pipes: apply a held key/button, send a malformed or truncated command, then close stdin. Verify fatal cleanup releases native input and reaps FFmpeg; parser rejection by itself is not sufficient. Verify abrupt-death outcomes rather than assuming graceful code ran after termination.
- Use native snapshots to establish app state. A short real-browser check proves typing, dragging, scrolling and key combinations reach the app; native cua-driver input alone does not prove browser delivery.
- Retained SDK tests keep double-click unlocked. Verify manual pointer lock separately without changing the focus/control policy.

No release-applied acknowledgement or automatic daemon recovery is added to satisfy these tests. If ordered handoff proves insufficient for a required behavior, return the evidence before changing the protocol.

## Slice 5: resize and exchange clipboard text

### Changes

- `crates/streamd/src/wayland/output.rs` and native encoder state: apply resize commands, preserve request IDs and emit the existing resize-applied event with its generation. Distinguish accepted output size from encoded size and first presentation.
- `crates/gateway/src/session.rs` or a cohesive private quality module: preserve the reference feedback policy and encode the existing quality command. Native validation remains independent. No new adaptive modes.
- `crates/gateway/src/daemon.rs` and video state: handle resize/restart metadata, discard stale correlation/bootstrap, and send generation-specific readiness through the ordered writer. Interleave this traffic safely with input rather than introducing a second pipe writer.
- `crates/streamd/src/wayland/clipboard.rs`: own MIME selection, offers, bounded nonblocking FD transfers, cancellation and current text state. Encode/decode the retained clipboard IPC payloads.
- Gateway browser/event handlers: preserve clipboard JSON and resize/quality messages. Keep browser permission failures recoverable at the browser boundary. Large clipboard events may not stall command dispatch or grow metadata queues indefinitely.

### Validation

- Tests cover pixel/stride/length overflow, rapid resize, stale acknowledgements, old encoder output, delayed readiness, quality transitions, malformed clipboard payloads, transfer cancellation and event backpressure.
- Run native/browser `--suite resize-clipboard`. Resize the browser at the existing 1280×900 and 1600×1000 sizes; output and encoded dimensions must converge and the whole desktop remain visible.
- Verify text in both directions, empty selections, disconnect during transfer and clipboard permission denial. Simultaneous clipboard and frame events must retain valid framing and metadata order.
- Resize followed by idle must display the final new-size frame. Old-generation frames must not resize the canvas back.

The accepted raw-pixel ceiling remains 3840×2160 within the dimension envelope. Both receiving boundaries validate their relevant limits; delivered FPS is measured separately from the configured target.

## Slice 6: preserve cursor behavior

### Changes

- `crates/streamd/src/wayland/cursor.rs`: ext-image capture, virtual-pointer seat-capability ordering, one pending frame, bounded SHM, image/hotspot pairing, visibility and deduplication.
- Emit the patched version-2 cursor image/visibility events through the single stdout writer. Retain their BGRA/premultiplied-alpha contract. Only replace pending cursor state where replacement preserves the current image/hotspot/visibility combination; do not apply this policy to frame metadata.
- `crates/gateway/src/daemon.rs`, `cursor.rs`: independently validate cursor event sizes/fields, convert pixels to PNG, cache the latest combined state and send existing controller messages. No native buffer pointers cross the pipe.
- Restore current state for a controller attaching while idle. Clear it when the native session ends and initialize it afresh on the next session; this does not imply transparent daemon restart inside a live gateway.
- Keep the retained browser's scaling, hidden cursor and late-decode guards. Use the committed PNG samples as evidence and port the reference alpha/channel tests.

### Validation

- Native tests cover startup ordering, unsupported formats, bounds, image/hotspot pairing, cancellation and duplicate suppression.
- Gateway and real-pipe tests cover fragmented cursor events, truncated/oversized payloads, alpha conversion, replacement/reset, idle attachment and cursor traffic mixed with ordered metadata.
- Run retained browser tests and native/browser `--suite cursor`: text, link, resize, observed hide/show, reconnect without movement and ordinary use without pointer lock.

Keep Breeze and the currently supported output/SHM formats. Do not add role classification, compositor patches or automatic pointer lock. State which hiding behavior was observed; enter/leave is not universal proof of application cursor hiding.

## Slice 7: verify combined failure and comparison

Bounds and cleanup arrive with their owners in earlier slices. This slice checks combinations, not the first implementation of safety behavior.

### Changes

- Complete `probes/rust-desktop/native-check.ts` and `smoke.ts` with `--suite recovery` and `--suite all`. Report prerequisites and bounded outcomes, and distinguish unperformed manual checks from passes.
- Extend real-pipe and paired-process cases across gateway exit, daemon exit, FFmpeg restart, blocked stdin/stdout, malformed peer output, resize during backlog, input loss, clipboard/cursor traffic and slow viewers.
- Kill only test-owned PIDs/service instances. Gateway shutdown cleans up the daemon tree; daemon EOF/shutdown cleans up input and FFmpeg. Test abrupt gateway death and forced cleanup escalation, not just normal exits.
- Keep fatal daemon loss as whole-session failure. Service-level restart may start a new pair; preserving the gateway and transparently restarting a native worker is outside this port.
- `apps/stream-viewer/` recorder integration and `probes/rust-desktop/` evidence: label new records as Rust and save both binary identities, environment and topology. Preserve the four original recordings and keep new recordings optional for the by-eye verdict.

### Validation

- Run shared gates, the explicit real-FFmpeg group, gateway IPC checker, proxy tests and `cargo build --release --workspace`.
- Run both isolated probes with `--suite all` using the matched Rust pair. Verify fresh keyframes after encoder restart, no cross-generation metadata pairing, bounded failure on pipe stalls and no surviving owned FFmpeg child after shutdown/fatal session failure.
- Verify actual held-input cleanup after abrupt peer loss. If the daemon or compositor fails to release it, record a failure rather than asserting that a killed process executed cleanup.
- Check the gateway has no Wayland runtime link and the daemon has no browser listener. Document that inherited user/environment permissions remain unhardened. Do not claim a denied-access security test passed when no such restriction was implemented.
- Run `pnpm --filter @sprite-desktop/viewer check`, `test` and `build`, plus recorder tests and probe typechecking, for regressions in shared tooling and the retained VNC viewer.
- Josh makes one short comparison with the unchanged Waymote desktop: typing, dragging, resize, clipboard, cursor and reconnect. Keep subjective feedback separate from counters. No required long acceptance campaign or inferred physical latency measurement.

A passing port is a usable isolated two-process Rust desktop with evidence. It is not a release, migration or completed privilege-separation project.

## Isolation and executor handoff

All remote tests target a new disposable `sprite-desktop-rust` after deployment authorization. Require `auth: sprite` and `private_access: admins`; reject protected names and unexpected ownership before mutation. Tests do not silently create Sprites or install prerequisites.

Do not change `josh-desktop`, `sprite-desktop-v1`, `sprite-desktop-waymote`, the VNC comparison service, installer records or the saved source patch. Build the paired Rust artifacts together; do not introduce runtime version negotiation, a reference-daemon fallback or an alternate media transport.

A mixed-language pair can be an explicit diagnostic against the pinned reference if useful, but it is neither the shipped configuration nor a substitute for the full Rust gate. No mixed-pair compatibility promise is added.

The executor plan must stamp a fresh jj revision and compare scoped paths before editing. Keep unrelated working-copy changes and paused release work intact. Resolve crate versions, native build prerequisites, exact wire constants, byte budgets and test deadlines before executing the affected slices. Native/media checks that need capabilities must say which ones and fail clearly when they are absent.

## Review gate

The architecture is settled. Review the seven slices, especially the split between component/hostile-peer tests in slice 2 and the actual paired Wayland handshake in slice 3.

Stop for evidence and design review if the retained IPC cannot support a required behavior, bounded correlation cannot be made correct, a release-applied acknowledgement becomes necessary, or deployment would change private access/protected desktops. Do not quietly add protocol extensions, privilege mechanisms or a shared implementation library.

Implementation is present. Finish the unperformed paired failure checks recorded in `probes/rust-desktop/README.md` before accepting all seven slices. Keep VNC release and migration work paused.
