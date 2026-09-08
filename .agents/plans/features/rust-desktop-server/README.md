# Private Rust desktop server

Status: the original repository now contains one Socket implementation, but the candidate is local and has not been deployed. VNC, noVNC, the Worker/proxy bridge, shared tickets, WebRTC code, and selectable capture alternatives were removed from the active tree. WLR capture is restored; audio was removed. Neither retained Sprite was touched.

Josh chose Socket for this private project: WLR screencopy, H.264 over loopback RTP, a same-origin WebSocket gateway, WebCodecs, and owned TypeScript viewer code in `apps/web`. The two Rust processes remain separate: `sprite-desktop-streamd` owns Wayland, FFmpeg, and desktop control; `sprite-desktop-gateway` owns HTTP, WebSockets, daemon supervision, RTP/metadata correlation, and bounded subscribers.

## Plan records

- [001-design-discussion.md](001-design-discussion.md): historical accepted design and initial Rust port boundaries.
- [002-structure-outline.md](002-structure-outline.md): historical seven-slice port outline.
- [003-socket-next-steps.md](003-socket-next-steps.md): accepted five-slice consolidation and verification outline.
- [004-implementation-status.md](004-implementation-status.md): current implementation, evidence, and open gates.

## Current result

The normal web app, release builder, installer, gateway, and daemon now use Socket. The local compositor harness passed quiet-idle, resize, background return, real network interruption, held-input release, native canvas capture, and codec/color checks at 30 and 60 FPS. The release archive contract uses only the two Rust executables and checks the process-group owner. Deterministic embedded-asset timestamps make repeated builds from fixed source match.

The evidence does not close the plan. The moving fixture produces about 24.5 visual updates/s and has no unique visual IDs, physical-presentation check, or p95 freeze proof. No fresh private Sprite has exercised the ordinary installer, health/authentication boundary, clipboard, IME, cursor, historical motion fidelity, latency, or target devices. Any provisioning requires an agreed cloud-use allowance first.

See [004-implementation-status.md](004-implementation-status.md) for the five-slice map and [`probes/socket-local/RESULTS.md`](../../../../probes/socket-local/RESULTS.md) for local measurements.

## Boundaries

This work does not add transport fallbacks, release migration machinery, audio, a generic gateway, or broad browser support. Preserve historical source in jj history and the exact prior binary pair in `probes/rust-desktop/baseline/retained-socket.tar.gz`. A new deployment needs explicit authorization and must not replace either retained desktop by implication.
