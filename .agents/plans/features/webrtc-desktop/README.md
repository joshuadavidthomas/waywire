# WebRTC desktop through the existing Sprite page

Status: exploration concluded. Josh chose Socket as the implementation for this
private project. Preserve WebRTC's findings and source history; do not maintain it
as an alternative runtime. Consolidate the repository onto Socket.
The CPU comparison design was accepted and its first bounded pilot is recorded.
The first pilot found excess WebRTC CPU while viewing a quiet fixture. That defect
is fixed, and a second batch confirmed the idle-cost reduction. Equal-quality
comparison remains incomplete. The implementation remains an exploration, not
permanent provider selection or production cutover.
The original design was planned on 2026-09-07 at jj change `nkspolspoyvu`,
reconnaissance revision `b67d9106626e`.

## Purpose

Replace the custom video player with WebRTC and native browser video while
preserving the existing private Sprite URL as the entire user interface. Account
for networking, secrets, lifecycle, cost and acceptance before another trial.

## Artifacts

| Document                                             | Status                   | Purpose                                                                    |
| ---------------------------------------------------- | ------------------------ | -------------------------------------------------------------------------- |
| [001-design-discussion.md](001-design-discussion.md) | Accepted for exploration | Project-specific deployment, managed relay, ownership and required proof   |
| [002-structure-outline.md](002-structure-outline.md) | Accepted                 | Five implementation slices, trial limits, network-first proof and rollback |
| [003-comparison-design.md](003-comparison-design.md) | Accepted; pilot incomplete | Paired sandbox and client CPU tests, quality checks, and decision rules |
| [First CPU pilot](../../../../probes/desktop-comparison/RESULTS.md) | Historical pre-repair results | Separate server/client costs, readiness defect, failed runs and cleanup |
| [Readiness remeasurement](../../../../probes/desktop-comparison/READINESS-RECHECK.md) | Repair measured; temporary Sprite deleted | Three paired quiet/motion rounds, input wakeup checks and remaining limits |

## Requirements for the approved exploration

- WebRTC was approved for exploration. Socket is now the chosen implementation. Preserve the evidence and source history; remove alternative runtime wiring during Socket consolidation.
- Capture, encoding, the portal and session logic stay on the Sprite.
- Preserve two production Rust executables and the existing desktop controls.
- No local proxy, CLI, extension or manually configured client relay.
- No lower fidelity or weaker acceptance implied by a successful connection.

## Accepted exploration

Use the existing Rust gateway for same-origin signaling and managed Cloudflare
Realtime TURN for encrypted media relay. The external service and its operating
requirements are approved for the proof. Other providers and self-hosted TURN
remain future options; no multi-provider framework is needed now. The current
native dependency supports TURN over UDP only; the proposed Sprite-to-relay path
has worked in bounded native playback tests. Browser TCP/TLS relay support does
not establish native TCP/TLS support.

## Implementation progress

Code is in the isolated jj workspace `webrtc`, at `../sprite-desktop-webrtc`
(change `rpxmlzxt`). WebRTC runs on its separate `sprite-desktop-webrtc` Sprite;
`sprite-desktop-rust` retains the Socket desktop. Later isolation decisions replaced
the outline's original proposal to switch the retained service in place.

TURN authority and native media now work. The old API denials remain recorded in
`probes/webrtc-exploration/results/`; they are historical failures, not the current
blocker. Playback, reconnection and VP9 packet fixes are recorded in
[the hands-on report](../../../../probes/webrtc-exploration/HANDS-ON-2026-09-08.md).
That report contains bounded trial observations, not a promise of current uptime.

Josh reports crisper text with WebRTC and comparable basic interaction in both.
His WebRTC viewer still appears capped at 30 FPS; smaller or different client
samples do not resolve that report. No matched sandbox/client CPU advantage has
been established. Full replacement, broad network support, physical latency and
long-session qualification remain unproven.

## Next gate

The approved 55-minute pilot has ended and its temporary Sprite is deleted.
Read [the pilot report](../../../../probes/desktop-comparison/RESULTS.md) before
running another batch. The fresh-viewer readiness defect is fixed and
[remeasured](../../../../probes/desktop-comparison/READINESS-RECHECK.md). Quiet
sandbox cost is now nearly equal; motion CPU and presentation rates still differ.
The second temporary Sprite is also deleted. Both retained desktops stay untouched.
Do not start another broad comparison or expand WebRTC by default. Continue with
[the Socket next-steps outline](../rust-desktop-server/003-socket-next-steps.md).
Revisit WebRTC only if a concrete Socket limitation or changed viewing needs
justify its operational cost.

Sandbox CPU remains the primary concern; full browser CPU is separate. Bandwidth
is secondary. The 30 FPS resource-budget arm does not weaken the historical 60 FPS
or fidelity criteria. Production replacement still needs separate acceptance.

The existing two-process decisions live in
[the Rust server bundle](../rust-desktop-server/README.md). Earlier codec and network
experiments remain in [the exploration report](../../../../probes/webrtc-exploration/README.md).
