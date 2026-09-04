# Private Rust desktop server

Status: implemented and deployed to the authorized private `sprite-desktop-rust` trial. Basic desktop checks pass. Performance corrections measured roughly 50 FPS at 1824×848 with improved color fidelity; sustained 60 FPS and combined failure acceptance remain incomplete. See [performance evidence and codec support limits](../../../../probes/rust-desktop/PERFORMANCE.md). See [live evidence and remaining checks](../../../../probes/rust-desktop/README.md#evidence-status).

Planned on 2026-09-05 at jj change `nkspolspoyvu`, working-copy revision `c4b799472264`. The revision records reconnaissance before these files were added; the existing working copy contains substantial VNC and Waymote experiment work.

## Purpose

Own the desktop server in Rust inside this repo. Use the working patched Waymote desktop as a reference while retaining the browser decoder and FFmpeg. A private port can establish useful behavior before any release or migration decision.

## Accepted direction

- Replace the Zig daemon and Go gateway with two Rust executables. Preserve their process boundary, daemon IPC and loopback RTP.
- Keep the owned server and browser code in this repository.
- Use TypeScript and Effect for the owned viewer. Retain WebCodecs and FFmpeg rather than rewriting the decoder, codec or desktop compositor.
- Keep existing desktops and the current Waymote reference unchanged during the experiment.
- Carry attribution in the README and LICENSE. Keep reference details in the existing Waymote probe docs; do not add `third_party/`.

## Artifacts

| Artifact                                             | Status                            | Purpose                                                                                             |
| ---------------------------------------------------- | --------------------------------- | --------------------------------------------------------------------------------------------------- |
| [001-design-discussion.md](001-design-discussion.md) | Accepted                          | Scope, repository layout, runtime ownership, media boundary, cursor limitations and verification    |
| [002-structure-outline.md](002-structure-outline.md) | Accepted; verification incomplete | Seven two-process implementation slices, including pipe, media-handshake and child-lifecycle checks |

Josh authorized implementation from the revised outline, then directed desktop verification on a Sprite. Implementation proceeded without a separate executor plan.

## Accepted design decisions

- Two private binary packages: `sprite-desktop-streamd` in `crates/streamd/` and `sprite-desktop-gateway` in `crates/gateway/`. Neither links the other's implementation.
- The daemon owns Wayland access and FFmpeg supervision. The gateway owns browser networking, daemon supervision, RTP/metadata correlation and bounded video subscriptions.
- A strict TypeScript/Vite viewer with Effect-owned session and permission tasks in `apps/stream-viewer/`. Leave VNC's `bridge/` and `apps/viewer/` alone.
- Retain the patched reference's version-2 daemon command/event protocol and keyframe handshake. FFmpeg sends RTP to the gateway over loopback.
- Preserve separate address spaces and a boundary for future OS hardening. Separate identities, permissions and sandbox restrictions remain a later task; the port does not claim enforced privilege isolation.
- Establish parity with the current patched cursor images and control policy. Laptop-native cursor roles and engagement behavior remain a separate UX decision.
- Omit server audio and broad platform support from the private port. Use only the authorized disposable `sprite-desktop-rust` trial.

## What better means

A maintainable Rust server renders and controls the real desktop through the retained browser, with responsive resize, clipboard, cursor shapes and reconnect. Queue limits, generations, held input and subprocess cleanup remain correct. A working first frame alone is insufficient, and a Rust port does not by itself establish better latency.

## Implementation routing

The revised outline follows the real process boundary:

1. Own the browser and pin both IPC/browser contracts.
2. Implement native media and gateway process plumbing, with real-pipe/hostile-peer checks.
3. Show the actual desktop through both Rust binaries and their keyframe handshake.
4. Control the desktop through ordered IPC, without claiming a release-applied acknowledgement.
5. Resize and exchange clipboard text.
6. Preserve cursor events and browser state.
7. Verify combined failures and compare the result.

Test peers are fixtures, not alternate deployed daemons. The paired Wayland gate requires both real Rust executables. Queue bounds and child cleanup arrive with their owners; hardening remains separate. No separate executor plan was written. The live trial does not establish every failure gate in slice 7.

## Explicitly outside this effort

No VNC cutover, release publication, installer redesign, Cloudflare deployment, codec implementation, Rust/Wasm frontend, or changes to the reference Sprite. Native-feeling cursor semantics must not be claimed as solved merely because the server uses Rust.
