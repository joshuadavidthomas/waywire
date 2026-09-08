---
type: structure-outline
repo: sprite-desktop
status: in-review
planned_at: 2026-09-08
socket_reference_revision: 6bd58a5f
webrtc_reference_revision: b358ea61
source_design_discussion: 001-design-discussion.md
source_decision: Josh chose Socket as the implementation for this private project. Remove discarded approaches rather than maintain multiple stacks or invent release-migration requirements.
---

# Next steps for the Socket desktop

## Direction

Socket is the chosen implementation: H.264, WebSocket, WebCodecs and the tested WLR capture backend. Make the repository consistently implement that choice. Remove VNC/noVNC, its bridge and alternative runtime wiring. Preserve useful experimental findings and source revisions in history, not as maintained implementation choices. Keep the two native executables and direct agent screenshots independent of human viewing.

This is a private project whose exploration has selected an approach. There is no shipped product, public upgrade contract or compatibility obligation to invent. Consolidating setup, the application and build scripts onto Socket is part of this work, not a later migration project.

This outline proposes five slices. Planning changes only these documents. It does not authorize interrupting either retained desktop, provisioning another Sprite or deleting provider resources.

When a slice is authorized, implementation includes its regression tests and required measurement. Do not stop after a local patch and present remeasurement as an unrelated task. Agree on a bounded remote allowance before starting any slice that needs cloud verification; finish the tests and cleanup within that allowance.

## What the evidence says

- The repaired WebRTC build no longer burns a full core while viewing a quiet desktop. Three paired runs measured about 0.026 Socket sandbox cores versus 0.028 WebRTC cores. Slow scripted work also had a small absolute CPU gap. See `probes/desktop-comparison/READINESS-RECHECK.md` in the WebRTC workspace.
- During motion, Socket used less CPU but showed fewer updates: roughly 42 canvas draws/s versus 50 native-video callbacks/s. One Socket run reported large lateness. These are investigation targets, not proof of an encoder or transport cause.
- The earlier Socket 30 FPS attempt stalled in the FFmpeg/metadata pipeline. Its cause is unknown. It is separate from Josh's WebRTC 30 FPS observation.
- Both viewers delivered input after idling once the test waited for control authority before focusing the remote application. Preserve that distinction in future probes.
- The retained Socket daemon uses WLR screencopy. The original workspace currently builds experimental ext image-copy capture. Its gateway/viewer still use Socket; they are not WebRTC. The WebRTC workspace changes both transport and codec.
- The repository's setup scripts and application still contain VNC wiring from the earlier exploration. Replace that wiring with Socket and remove the abandoned implementation. Its presence is unfinished consolidation, not a reason to preserve VNC.

## Workspace and evidence map

- Original repository: `/home/josh/projects/joshuadavidthomas/sprite-desktop`, jj revision `6bd58a5f` at planning time.
- WebRTC experiment: sibling `sprite-desktop-webrtc`, jj revision `b358ea61` at planning time. This file lives there alongside the latest measurements.
- Saved WLR source: original repository's `target/keyframe-trial-20260906/source`. Its base is `3f54ee99c5af84f8150a78066b5ef2b55f013c5f`; the saved directory already includes the retained round-six patch.
- Patch against the historical base: `probes/rust-desktop/round6-wlr.patch` in the original repository. Do not apply it twice to the saved source.
- Retained daemon: `target/performance-round6-pipe1m/sprite-desktop-streamd`, SHA256 `3341878c62241739569ec6b9b0e435afea1fba12050da5914040f5bca92bab14`.
- Retained gateway, including its embedded viewer: `target/performance-round3-ext/sprite-desktop-gateway`, SHA256 `0c771df4c0fb48d99971efc3133b52a7b3f0dd0474812bd8cd3ba72ddaa6abb4`. The directory's name does not identify the deployed daemon's capture backend.
- Both complete benchmark binary pairs are also archived under the WebRTC workspace's `target/desktop-comparison-7994fabbfb83/binaries/` and `target/desktop-comparison-a9459d09ba51/binaries/`. These directories are ignored and must not be the only durable record.

Before implementation, run `jj diff --from 6bd58a5f -- crates apps/stream-viewer probes/rust-desktop package.json` in the original workspace and `jj diff --from b358ea61 -- crates apps/stream-viewer probes/desktop-comparison` in the WebRTC workspace. Reconcile changed assumptions rather than overwriting another change. Historical line references below are navigation aids; confirm symbols after selecting the Socket source base.

## Slice 1: Make the tested Socket build reproducible

### Outcome

A documented, buildable H.264/WLR baseline becomes the basis of the single Socket codebase. Preserve the experiment's revision and evidence before removing alternative implementations. A separate workspace may isolate this work safely; it is not another permanent product variant.

### Work

1. Preserve the experiment's source revision, native binary hashes, configuration provenance, comparison reports and owned synthetic fixtures. Keep secret/runtime authority files out of tracked archives. Do not revoke shared TURN authority or restart the retained WebRTC Sprite.
2. Reconstruct the daemon from the historical WLR revision plus the recorded patch. A fresh workspace from `3f54ee99c5af84f8150a78066b5ef2b55f013c5f` is the starting point, not the current ext-capture worktree. Preserve later useful fixes only as reviewed follow-up changes.
3. Recover the gateway and embedded-viewer build recipe separately. `PERFORMANCE-ROUND6.md:93-128` proves daemon reproduction; it does not prove the same recipe reproduces the retained gateway. Trace `PERFORMANCE-ROUND3.md:90-103`, the saved binary and jj history. Record any unresolved source/asset mismatch before proceeding.
4. Make the chosen source, lockfiles, viewer assets and build commands the normal Socket development baseline. Do not leave the only usable source under `target/` or add runtime switches between WLR/ext/VP9 just to retain experiments.

### Verification

The historical daemon reproduction command, from the original repository, is:

```sh
cargo build --locked --release -p sprite-desktop-streamd \
  --manifest-path target/keyframe-trial-20260906/source/Cargo.toml \
  --target-dir target/keyframe-trial-20260906/build
sha256sum target/keyframe-trial-20260906/build/release/sprite-desktop-streamd
```

Compare with the retained daemon hash above. Different compiler/build inputs may change bytes; document and resolve the difference instead of silently claiming exact reproduction. A newly built candidate must have its own identity.

The slice is done when another checkout can build the selected Socket source, run the local gates below, and identify precisely how it relates to the measured binaries. Gateway/embedded-viewer provenance must not remain an assumption.

## Slice 2: Make the repository consistently Socket

### Outcome

Normal setup, the application, health checks and build scripts all use Socket. VNC/noVNC, the RFB bridge and alternative runtime selection are removed. A new private desktop starts the chosen implementation without experimental commands or transport choices.

### Work and seams

- Replace Xvnc/RFB setup in `installer/desktop.sh` and `installer/install.sh` with the two Rust executables, compositor and required Socket dependencies. Keep private authentication and resource ownership intact.
- Wire the application in `apps/web` to the owned Socket viewer. Remove its VNC session implementation and any noVNC-only viewer package after checking its callers. Preserve useful controls and styling, not the old transport abstractions.
- Remove `bridge/` and VNC-specific proxy, ticket, port and socket wiring. Follow consumers through `apps/gateway`, shared packages and provisioning code so removal does not leave broken imports or dangling configuration. Retain mechanisms still needed for private access; do not confuse removing RFB with removing authentication.
- Update `scripts/build-release.ts`, package/workspace manifests and setup/build commands to produce and start Socket artifacts. This script's name does not imply an existing public release or upgrade obligation.
- Remove obsolete dependencies, tests, configuration and active documentation. Keep experimental evidence clearly historical and preserve source in jj history. Do not carry WebRTC, TURN or ext-capture implementation paths into the normal Socket application merely to retain the experiment.
- Do not add fallback transports, compatibility aliases, dual parsers or an old-installation migration framework. None is required by the accepted project scope.

### Verification

Trace the ordinary setup and application entry points end to end. A fresh isolated desktop must start Socket, load its authenticated viewer, report actual runtime health, accept input, resize and exchange clipboard text. Removing VNC must not weaken private access.

Run the affected application checks plus the Socket gates below. Audit remaining references with `rg -n -i 'vnc|novnc|xvnc|rfb|webrtc|turn' installer apps packages scripts package.json pnpm-workspace.yaml`; review each match rather than deleting unrelated words or historical evidence blindly. No active entry point may still depend on a discarded transport.

The done condition is one working implementation throughout the repository. This consolidation does not wait for a separate release or migration design.

## Slice 3: Make monitoring and takeover survive ordinary failures

### Outcome

A quiet desktop remains cheap, changing work becomes visible, and interrupted sessions recover without stuck input or stale sockets. The 30 FPS pipeline failure has a reproducer and a verified resolution before that setting is described as supported.

### Source and test seams

- `apps/stream-viewer/src/sdk/waymote.ts`: `connectVideo`, `connectControl`, `handleVisibilityChange`, `releaseInput`, `disconnect` and decoder generation handling.
- `apps/stream-viewer/src/sdk/waymote.test.ts`: existing stale-socket, visibility, keyframe and queue-overflow tests around lines 1014–1506. Extend these actual Socket tests; do not substitute the separate noVNC connection controller.
- `crates/gateway/src/daemon.rs`, `http.rs`, `session.rs`, `video.rs`: child lifetime, controller lease, bounded writers, GOP recovery and metadata correlation.
- `crates/streamd/src/video.rs` and `wayland.rs`: idle encoding, pipe backpressure, generation changes and native input release.
- `probes/rust-desktop/ipc-check.ts:522-874`: real gateway checks for admission, disconnect cleanup, blocked pipes and slow control output. Use these real boundaries before adding mocks.

### Checks that must accompany fixes

- Idle with no viewer; attach; idle with viewer; type; close; reattach. Confirm actual new pictures and saved input, not only green connection status.
- Background for 35 seconds, then return. Repeat with transport loss while hidden. Pending connections from the old session must never take ownership.
- Disconnect while a key/button is held; verify release in the native application, then allow a replacement controller to type. Exercise preedit cancellation as well as ordinary keys.
- One responsive viewer plus a slow viewer. Keep the responsive viewer usable; bound the slow viewer's queues and release its resources on close.
- Encoder exit, daemon EOF and service restart. Verify either successful fresh-generation recovery or an explicit disconnected state; never leave an apparently healthy frozen picture.
- Run 30 and 60 FPS as distinct cases. Capture the configuration actually accepted by the daemon. Do not declare 30 FPS fixed merely because a 60 FPS rerun worked.

Add a regression before each repair. Run local IPC/media gates, then the bounded real-desktop case and a CPU repeat in the same slice. Preserve failed attempts. Local unit coverage alone does not close the real-compositor checks.

## Slice 4: Improve readable text without adding backlog

### Outcome

Text improvements have source/decoded evidence, and any presentation change reduces delay without sacrificing useful frames, native controls or idle efficiency.

### Work and seams

1. Establish paired source/decoded images for editor text, terminal output, colored small text and window borders at 1280×720 and the chosen normal-use resolution. Record DPR, CSS content dimensions, zoom and decoded dimensions before blaming the codec.
2. Separate scaling/resampling, color conversion, compression and stale-frame problems. The current dense-text still scores alone do not identify which needs changing.
3. Investigate the motion lateness through `expectedPresentationTime`, `renderFrame`, `scheduleVideoPresentation`, `decodeMessage` and the bounded frame queues in `apps/stream-viewer/src/sdk/waymote.ts`. Existing tests cover overdue-frame collapse and a 52.5 Hz capture sequence; do not replace them with a nominal FPS counter.
4. Change one measured cause at a time. If compression is the cause, permit one explicitly recorded higher-bitrate H.264 comparison before considering preset changes. Preserve full resolution, chroma, range and color tags. No codec change, sharpening filter or chroma downgrade as a shortcut.

Use `probes/rust-desktop/performance.ts:1614-1688` for unchanged native brackets and RGB PSNR. Use `frame-counter.ts`, `browser-observer.ts`, `browser-timings.ts` and delivery-analysis tests to distinguish received, decoded and drawn frames. Extend their fixed bounds deliberately when changing workload/resolution; do not claim arbitrary frame-ID validation from their historical defaults.

Still and motion fidelity passes are separate from CPU sampling. Moving source/decoded images need matching visual IDs. Keep the historical 35 dB RGB floor on its original fixture; dense-text scores from another fixture do not replace that check. Include text crops for human review.

Retain the historical 60 FPS qualification gates: at least 45 observed presentation updates/s, p95 gaps at most 50 ms and edge-inclusive freezes at most 250 ms on the controlled reference setup. Report unique-image and physical-presentation limits honestly. Lower CPU caused by discarded useful work is not an improvement.

## Slice 5: Verify the complete Socket desktop

### Outcome

The ordinary setup and application run the selected Socket implementation. An identified build has a short report covering ordinary use, recovery, readable text and separate server/client cost. Preserve the prior working binary pair in case a private deployment needs to be undone. There is no repeated transport bake-off.

Run the reliability cases and decisive text/motion checks together on the candidate, including clipboard, IME, cursor shape and resize. Recheck the most important cases on Josh's browser/device if it differs from the controlled Linux client. Keep direct agent screenshot behavior unchanged.

Use an isolated desktop for destructive/failure tests. Before provisioning, agree on a lifetime/spending bound and start the timer at creation, including setup. Do all local validation first, verify resource/accounting boundaries, and run owned cleanup before reporting. Do not reuse old `performance.ts --restart-trial` examples that address `sprite-desktop-rust`: they would interrupt the protected desktop.

After acceptance, deploy only to an explicitly selected target with the prior binary pair available for rollback. This outline does not itself authorize replacing either retained desktop.

## Verification commands

These scripts were confirmed in the original repository's `package.json`. Confirm they still exist in the selected Socket base before using them; do not run its H.264 IPC expectations against the WebRTC branch.

```sh
pnpm --filter @sprite-desktop/stream-viewer check
pnpm --filter @sprite-desktop/stream-viewer test
pnpm check:rust
pnpm test:rust:ipc
pnpm test:rust:media
pnpm build:rust
pnpm check
pnpm test
pnpm build
```

`check:rust` includes the viewer build, Rust formatting, Clippy, Rust tests and probe tests. `test:rust:media` runs the explicitly ignored real-FFmpeg tests; default test success is not a substitute. Validate new comparison TypeScript with `pnpm exec tsc -p probes/desktop-comparison/tsconfig.json` where those probes are retained. Record baseline failures instead of relaxing gates to hide them.

The verification standard is the project's `coding-standards` guidance: test the actual claimed behavior through the protocol, process and browser boundaries. Test fixtures must not turn into alternate production implementations.

## Boundaries and handoff

- Recover the baseline, consolidate the repository onto Socket, fix reliability, improve clarity, then verify the whole workflow. Each slice includes its own tests and required measurement.
- No further WebRTC feature work, ext-capture migration, provider swap, generic transport layer, audio expansion or compatibility framework. Removing discarded implementations is explicitly in scope.
- Preserve source history, retained binaries and unrelated work before changing the codebase. Historical experiments do not need to remain active packages, dependencies or runtime branches.
- If source/asset provenance cannot be recovered, a failure cannot be reproduced, or a proposed fix changes codec/security/ownership contracts, stop that slice and write `memo-<question>.md` here. State the observed state, intended outcome and unresolved choice. Do not invent a causal explanation or silently broaden the task.
- An implementation handoff must include what was measured, what failed, remaining limits and cleanup evidence. A passing build alone is not the end of a performance/reliability slice.

## Review gate

Socket is selected, and repository consolidation is part of the accepted direction. Review the execution details, not the transport choice. Start by recovering the tested baseline; then make setup and the application consistently Socket. Resolve concrete source/asset questions during that work without reopening a release or compatibility discussion that this private project does not require.
