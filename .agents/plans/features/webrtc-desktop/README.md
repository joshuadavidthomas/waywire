# WebRTC desktop through the existing Sprite page

Status: outline accepted; implementation authorized directly from it. Josh approved managed
Cloudflare TURN to prove the browser-only experience. This is not a permanent
provider decision or production-cutover approval. Planned on 2026-09-07 at jj
change `nkspolspoyvu`, reconnaissance revision `b67d9106626e`.

## Purpose

Replace the custom video player with WebRTC and native browser video while
preserving the existing private Sprite URL as the entire user interface. Account
for networking, secrets, lifecycle, cost and acceptance before another trial.

## Artifacts

| Document                                             | Status                   | Purpose                                                                    |
| ---------------------------------------------------- | ------------------------ | -------------------------------------------------------------------------- |
| [001-design-discussion.md](001-design-discussion.md) | Accepted for exploration | Project-specific deployment, managed relay, ownership and required proof   |
| [002-structure-outline.md](002-structure-outline.md) | Accepted                 | Five implementation slices, trial limits, network-first proof and rollback |

## Settled requirements

- WebRTC is the target. The user needs only the web page.
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
still needs proof. Browser TCP/TLS relay support does not establish native
TCP/TLS support.

## Implementation progress

Code is in the isolated jj workspace `webrtc`, at `../sprite-desktop-webrtc`
(change `rpxmlzxt`). The relay-only checker is implemented and tested locally:
20 Rust unit tests, four real subprocess tests, strict TypeScript, Clippy, release
build and the existing HTTP shutdown test passed. This is partial Slice 1, not a
working Sprite WebRTC demo.

Live qualification is blocked: Wrangler's `personal` profile lists the personal
Cloudflare account, but its OAuth scopes omit the required `Calls Write` permission.
After reauthorization, the direct TURN-key API returned HTTP 403/code 10000.
Josh then supplied a custom token in the original workspace's ignored `.env`.
That token verifies as active, but TURN access still returns HTTP 403/code 10002
(permission denied). Confirm the token's `Calls Write` permission and personal
account restriction; the file is already available. Both access results are saved
under the candidate's `probes/webrtc-exploration/results/`. No TURN resource was created and no Sprite
operation ran in this implementation step. The saved WLR binary hashes match the
outline. Native capture and production gateway/viewer changes have not started.

## Next gate

Implement directly from the accepted outline, as Josh requested.
The first slice proves Sprite TURN/UDP and browser TURN/TLS without touching
capture or the live viewer. No separate executor-plan gate remains. The current Rust/WLR deployment remains the rollback baseline.
Production replacement and changes to the quality gates require separate acceptance.

The existing two-process decisions live in
[the Rust server bundle](../rust-desktop-server/README.md). Local codec proof and
the failed Sprite connection are recorded in
[the exploration report](../../../../probes/webrtc-exploration/README.md).

The outline covers relay qualification; authenticated session ownership; native
VP9 and browser video; recovery/expiry/input safety; and real-page qualification
with rollback. Production changes wait for the prerequisite relay and media gates.
