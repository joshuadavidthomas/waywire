---
type: structure-outline
repo: sprite-desktop
change: nkspolspoyvu
revision_at_reconnaissance: a3120038de03
date: 2026-09-07
status: accepted
source_design_discussion: 001-design-discussion.md
---

# Implementation outline: browser-only WebRTC exploration

## Scope and review gate

Josh approved Cloudflare TURN for an exploration. Implement the accepted design
in five slices, with network feasibility first. The deliverable is an actual
interactive desktop at the existing private Sprite URL, with no local helper.
A local codec test, TURN allocation alone or first video frame is not that deliverable.

Josh accepted this outline and explicitly requested implementation directly from
it. No separate executor-plan gate remains. The limits and stop conditions below
govern execution.

## Proposed operating limits

These are concrete defaults for the exploration, not permanent product policy.

| Item                    | Proposed limit or rule                                                                                                                                     |
| ----------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Target                  | Only `sprite-desktop-rust`; retain `auth: sprite`, `private_access: admins`                                                                                |
| Provider                | One dedicated Cloudflare TURN key in Josh's selected account; no paid-plan upgrade without asking                                                          |
| Viewing                 | At most two admitted viewers and one controller; qualify one viewer for the first demo, then two-viewer handoff before replacement; Chrome 152/Linux first |
| Trial window            | One-hour absolute deadline recorded at activation; process restart must not reset it                                                                       |
| Viewer session          | At most 30 minutes; show expiry in the page and release cleanly                                                                                            |
| Relay grants            | Separate browser/server grants, 15-minute TTL; begin renewal at 10 minutes; revoke on exit                                                                 |
| Sprite Task             | One shared 90-second hold per gateway incarnation, renewed every 30 seconds while admitted sessions remain                                                 |
| Setup/reconnect         | 30-second setup deadline; automatic reconnect has a 60-second total budget                                                                                 |
| Admission               | At most six setup attempts/minute across this single-user gateway; closing and retrying does not reset the limiter                                         |
| Signaling               | SDP at most 64 KiB; at most 32 remote candidates and 64 KiB aggregate candidate text per negotiation; existing input/clipboard limits remain separate      |
| Provider/Tasks requests | Five-second deadline per request; bounded retries inside the owning session deadline                                                                       |
| Cost review             | Record dedicated-key usage before/after each run; stop further runs at $1 of incremental TURN usage or unexpected billing terms                            |

The time, admission and issuance limits bound the legitimate trial. They are not
a guaranteed dollar cap against stolen credentials or delayed provider accounting.
TURN credentials delegate relay bandwidth beyond this desktop. Short expiry,
revocation and secret handling remain necessary. Existing allocations can have
separate lifetimes; verify those before claiming that credential expiry stops traffic. At the nominal 8 Mbps, a
30-minute video downlink is about 1.8 GB before overhead; count provider-reported
TURN-server-to-client egress rather than multiplying by the number of allocations.

A missing account choice, insufficient API permission or a required paid upgrade
is a setup handback. Do not silently use an unrelated account or existing TURN key.

## Slice 1: prove the Sprite's relay path

Deliverable: a saved, bounded network result proving both required relay legs
before native capture or the live viewer is changed.

### Pre-build gates

Confirm the account, permissions and terms before requesting any allocation.
Also establish reproducible WLR source and a viable bounded keyframe/RTCP path
before gateway restructuring. These can invalidate the implementation and must
not wait until Slice 3. The source and media checks below describe their evidence.

### Files and boundaries

- Add a relay-only entry to the private test tool in
  `probes/webrtc-exploration/src/`. It must not start its HTTP server, wf-recorder,
  FFmpeg or desktop input. This is a network prerequisite, explicitly not a demo.
- Add narrowly scoped setup/check/cleanup commands under
  `probes/webrtc-exploration/`. Select the account explicitly, create one owned
  TURN key, and record its non-secret identity and cleanup obligation.
- Use the current pinned Rust WebRTC/TURN dependency. Its UDP-only native TURN
  support is a gate, not something a browser TLS URL can repair.
- Stage runtime authority through a private, untracked file/secret mechanism.
  Only credential-generation authority goes to the Sprite. Provisioning/account
  authority remains outside the desktop runtime. Keep it out of child environments,
  inherited file descriptors, arguments and artifacts.

### Verification

- Check current provider terms, account, Sprite egress policy and private/admin
  access. Inspect only the intended account and owned Sprite.
- Use owned test peers to prove allocation, authentication, permission/channel
  binding, relayed payload receipt and allocation refresh. Allocation success
  alone is insufficient. Cap this test at five minutes and 64 MiB of test traffic.
- Exercise the real Rust peer's relay path, not just a separate STUN script or a
  different WebRTC implementation. Then connect an owned browser test peer using
  relay-only ICE and only TURN/TLS on port 443. Verify relayed payload receipt and
  the selected browser transport. Prove Sprite UDP and browser TLS now, rather
  than deferring the second network dependency until after product-path work.
- This protocol preflight may use an in-memory, non-logging administrative
  signaling harness. It has no local listener or forwarding process. It proves
  network capability only; it does not qualify application signaling or the
  single-page user experience. Slice 5 must repeat the proof through the page's
  own signaling without that harness. Keep credentials out of process arguments.
- Preserve address-free route/type counters. Release allocations, revoke issued
  credentials and verify no probe process, task hold or new listener remains.
  Keep the existing service and output mode unchanged.

STOP if the Sprite cannot sustain UDP TURN with this dependency. Select a supported
native TURN implementation before proceeding. Do not patch around it with a local
proxy, a custom transport daemon, an unreviewed fork or a silently changed codec.

## Slice 2: one authenticated session owns signaling, media and control

Deliverable: the real gateway owns a bounded WebRTC session and its external
resources, with real local network fixtures proving the failure paths.

### Files and boundaries

- `crates/gateway/src/http.rs`: retain the embedded page and exact Origin checks.
  Use the existing `/control` WebSocket as the admitted session connection for
  both signaling and existing control messages. Its server-assigned connection
  identity already anchors the lease; no extra bearer ticket or loosely paired
  `/offer` and `/control` sockets are needed.
- `crates/gateway/src/peer.rs` (new): own native WebRTC negotiation, callbacks,
  candidate bounds, peer lifecycle and the connection's relay grants. Accept
  dependency-native SDP/candidate values after boundary parsing.
- `crates/gateway/src/protocol.rs`: add typed signaling messages and negotiation
  generations. Preserve input semantics; reject stale or out-of-phase messages.
- `crates/gateway/src/session.rs`: preserve `Sessions`, lease epochs and the
  ordered `ReleaseAll` boundary. Invalidate the old active epoch under the lease
  lock before enqueueing release, so the single writer rejects late requests
  carrying that epoch. Enqueue release before activating another owner. Preserve
  ordered native processing; do not add an applied-release acknowledgement or
  claim one exists. Gate acquisition/input on the same session's usable media
  state. WebSocket identity must not be replaceable by client input.
- `crates/gateway/src/turn.rs` (new): one Cloudflare-specific credential adapter.
  It translates provider responses into validated relay grants, renews/revokes
  them and classifies failures without dumping provider bodies or secrets.
- `crates/gateway/src/tasks.rs` (new): one serialized Tasks owner shared across
  admitted sessions. Follow `bridge/tasks.go` ownership semantics, including
  uncertain PUT results that still leave a release obligation, but use an
  incarnation-scoped Task name. A delayed old DELETE must not remove a restarted
  gateway's hold. Initial hold failure prevents admission; unresolved renewal
  failure closes sessions before the last confirmed hold can expire. Do not link
  the Go implementation.
- `crates/gateway/src/main.rs`, `Cargo.toml`: compose these owners, protected
  runtime configuration and the absolute trial deadline. No Wayland dependency
  enters the gateway and no shared native library is added.

The session progresses through setup, negotiation, awaiting first presentation,
active use and bounded recovery/closing. A new WebSocket always gets a new session
identity. ICE restart retains that socket/session but advances its negotiation
generation and revokes control until media is usable again. Every asynchronous
result belongs to its runtime, session and negotiation; stale work cannot restore
input authority. A connected transport alone does not permit input. On terminal
media failure, invalidate input authority and order release-all before another
controller can acquire it. A failed required release
write is a session/runtime failure, never permission to skip the handoff boundary.

Only the short-lived browser grant and its ICE settings travel through this
authenticated session. The provider key stays in the gateway. No public credential endpoint, token query string,
prototype loopback bridge or cross-origin API is part of the new runtime.

### Verification

- Existing private-Origin matrix plus missing, duplicate, null and foreign Origin
  cases must fail before allocating a peer or requesting a credential.
- Real HTTP and Unix-socket fixtures cover credential issue/renew/revoke, Tasks
  renewal/delete, slow response, malformed response, timeout and uncertain results.
- Cover cancellation after one of two grants is issued, stale provider responses,
  candidate flood, concurrent setup, phase-invalid input and late reconnect events.
  Secret sentinels must never appear in logs/evidence. Disable raw dependency and
  protocol logging; report bounded, typed failures instead of dumping SDP or ICE.
- Disconnect closes peer/control, stops renewal, attempts bounded revocation and
  releases the Task reference. Record failed revocation with remaining credential
  and allocation lifetimes; closing a socket is not credential revocation.
- Test absolute trial expiry across a process restart and session expiry while keys
  are held. Deadlines must not depend on an operator keeping a local script alive.

## Slice 3: real native capture reaches the native video element

Deliverable: the candidate contains one VP9/WebRTC media path and the existing
full desktop controls. No second capture process or old video decoder runs beside it.

### Source gate completed before Slice 2

The worktree contains experimental ext capture; the retained deployment is WLR.
Prepare the candidate in an isolated jj workspace and reconcile capture to the
recorded WLR source without erasing unrelated work or the preserved ext evidence.
Use `probes/rust-desktop/results/round6-builds.json`,
`probes/rust-desktop/results/round7-builds.json` and the saved source provenance
as inputs. Capture-backend tuning is outside this change.

Before proceeding, record the candidate source revision and verify the selected
capture implementation. A marker or matching `video.rs` alone does not prove the
whole daemon's provenance. If the intended WLR source cannot be reconstructed,
stop; do not deploy the default ext build and call it the baseline.

### Files and boundaries

- `crates/streamd/src/video.rs`: replace H.264 encoding with VP9 profile 1/full
  chroma. Retain the three raw buffers, replaceable pending frame, nonblocking
  one-MiB pipe, killable FFmpeg and explicit generation/SSRC changes. Keep `-re`
  unless a separate approved change permits removing it. Fix the motion bitrate
  overshoot and report actual delivered bandwidth.
- Change the current loopback RTP `pkt_size=60000` to a WebRTC-safe packet budget
  such as the prototype's 1200 bytes. TURN overhead and path MTU still need proof;
  do not forward 60-KiB packets onto an Internet peer.
- `crates/gateway/src/video.rs`: remove the H.264 FU-A/STAP-A and Annex-B-specific
  path. Use dependency-native VP9/RTP parsing, retaining bounded complete-frame
  validation, metadata correlation and generation handling. Do not add a codec
  implementation. Incomplete frames are discarded as units; never splice frames.
- `crates/gateway/src/peer.rs`: supply a coherent per-peer RTP sequence/timestamp
  space and negotiated payload/SSRC identity. A source encoder restart must not
  expose arbitrary timestamp/sequence resets as continuity in the same sender.
- `crates/gateway/src/daemon.rs`, both `protocol.rs` files and native capture:
  preserve source-generation/readiness coordination. Audit any codec-specific
  assumptions before claiming that the existing metadata pairing still works.
- `apps/stream-viewer/src/sdk/waymote.ts`, `cursor.ts`, `viewer-listeners.ts`,
  `client.ts`: attach a native video element, exchange typed signaling on
  `/control`, and retain mouse/keyboard/composition, clipboard, cursor, resize,
  pointer lock and controller UI. Adapt element types and letterboxed coordinate
  mapping; do not copy the prototype's incomplete input implementation.
- Delete the candidate's `/stream` video route, custom browser video header,
  WebCodecs queues, reset scheduler and canvas presentation path. Local codec
  fixtures and historical evidence may remain under probes. No H.264 fallback
  selector or local proxy is shipped with the candidate.

### Media contract gate completed before Slice 2

Damage-driven FFmpeg input timestamps advance by submitted frames; long idle gaps
are not necessarily wall-clock gaps in that stream. The gateway must derive the
WebRTC presentation timeline from the trusted capture clock and preserve its
continuity across idle and encoder generation changes. Source generation/SSRC
validates ingress; each peer sender owns its outbound SSRC, sequence space and
90 kHz clock. Idle time advances that clock. Test this explicitly;
blind RTP forwarding from the continuous synthetic prototype is insufficient.

The current IPC has keyframe-readiness and quality commands but no dedicated
force-keyframe command. Do not mistake readiness for a PLI/FIR response. First
check the native encoder control and whether the existing readiness/capture
mechanism can keep capture active until the next periodic GOP keyframe. That is
a bounded wait for a fresh frame, not an immediate force-keyframe operation.
Propose a two-second first-frame/injected-recovery deadline after transport is
ready, with refresh requests coalesced per source generation at no more than
four requests/second across peers. These are recovery bounds, not permission to
relax the ordinary 250 ms freeze gate. Any required IPC addition needs paired
wire tests. Do not restart the global encoder for every PLI or grow a second
encoder. If that contract cannot be met, return the design fork.

Likewise, delete browser decode-queue-driven adaptation rather than inventing
queue values from WebRTC statistics. Gateway feedback must have a defined effect
on encoder bitrate/keyframes and slow-peer isolation. No silent resolution or
chroma downgrade is part of this proof.

### Intermediate gates and verification

Complete these in order: valid native VP9 RTP and metadata; a bounded gateway
sender consuming it; the private page rendering it; existing browser input and
cursor/resize behavior operating against that video element. Do not debug all
four boundaries simultaneously.

- Real FFmpeg tests prove profile 1, full chroma, bounded packet sizes, bitrate,
  frame boundaries, random-access frames and clean restart/reaping.
- Real UDP/pipe tests cover packet loss/reorder/wrap, late metadata, mismatched
  generation/SSRC, no stale frames after resize and no final-frame dependence on
  a later packet. Preserve the metadata-before/after-RTP cases.
- Verify long idle then one changed frame and inspect library retransmission/
  pacing bounds instead of assuming its defaults are bounded. Two-viewer and
  slow-peer qualification follows the first single-viewer demo.
- Frontend tests preserve input, composition, cursor and letterbox mapping;
  unsupported VP9 profile 1 fails clearly without asking for local software.

## Slice 4: recovery, expiry and input safety

Deliverable: failure handling works before the user is asked to drive the desktop.

### Files and boundaries

- Extend `peer.rs`, `session.rs`, `turn.rs`, `tasks.rs` and the viewer session code.
- Extend `probes/rust-desktop/fixtures/streamd-peer.ts`, wire fixtures and IPC
  checks for any accepted protocol change. Preserve existing attribution.
- Add focused WebRTC process/browser checks under `probes/rust-desktop/` rather
  than teaching the old canvas recorder to report fictitious native counters.

### Verification

- Prove one real credential renewal with continued media, using a shorter test
  TTL if needed; default deadline arithmetic is also covered by controlled-clock
  tests. Confirm revocation and expiry behavior separately. Native and browser
  APIs may update allocations differently; configuration success is not proof.
- Exercise lost media with a working control socket, lost control with a live
  peer, ICE restart, provider failure, browser close and reconnect cancellation.
- Hold modifiers/buttons during failure and reconnect; observe native release
  before new input. Distinguish pipe-write completion from applied input. The
  broader two-viewer handoff matrix is a replacement gate.
- Keep an attached, otherwise quiet session alive across Task renewals. Close the
  final viewer and verify the Task is deleted or expires within its bounded lease.
- Exercise gateway shutdown and session cleanup before the demo. Assert no owned
  peer sockets, renewals or unjoined tasks remain. Confirm allocation deletion
  where the protocol acknowledges it; otherwise record the server-side expiry.
  Record uncertain credential revocation separately. Continue existing child/IPC
  regression tests; defer the expanded live fault matrix to replacement qualification.

## Slice 5: qualify through the real private page, then hand over the demo

Deliverable: a working, time-bounded WebRTC exploration link and a short evidence
record stating exactly what passed. Broader production acceptance is separate.

### Deployment and rollback

- Extend `probes/rust-desktop/deploy.ts` and `run.sh` for private TURN configuration
  and the trial deadline. Do not revive `apps/web`/`apps/gateway` or alter the VNC installer.
- Add a named administrative `webrtc-trial.ts activate|restore` operation under
  `probes/rust-desktop/`, using the existing ownership-checked deploy/restart seams.
  Before activation, save exact gateway/daemon bytes, embedded viewer, launcher,
  ownership marker, file modes/owners, service definition and output settings.
  Keep a durable off-Sprite backup in ignored `target/webrtc-trial-baselines/`
  with a non-secret manifest in the run evidence. Hashes alone cannot restore it.
  Installed/running identities must match the expected WLR pair; stop on drift.
- Upload the matched candidate and assets as one staged change. Only the existing
  owned `rust-desktop` service may be switched. No simultaneous live trials.
- Resize only through the owned gateway/daemon transaction, not an independent
  `wlr-randr` command against the active capture. The earlier external resize was
  followed by screencopy failure and a gateway exit.
- A failed gate triggers recorded rollback of exact saved files/definition, native
  health/identity verification and relay/task cleanup. Never rebuild the baseline
  from the current source. Do not kill processes by name or historical PID.

Expected baseline hashes at this outline's writing:

```text
gateway 0c771df4c0fb48d99971efc3133b52a7b3f0dd0474812bd8cd3ba72ddaa6abb4
streamd 3341878c62241739569ec6b9b0e435afea1fba12050da5914040f5bca92bab14
```

### Browser gate

Use an authenticated browser at the real private Sprite URL. Normal Fly login is
allowed; injected application tokens or the old local acceptance proxy do not
prove the browser-cookie path. Arrange that login before a timed trial if the
browser lacks a session. Never place credentials on the command line or record
cookie/token-bearing requests in artifacts.

The existing performance runner's local proxy is not this gate. A direct-URL
runner must not bind a local listener or route page/signaling/media through an
agent helper. Browser automation may observe and operate the page; the user-facing
session must work after that automation closes.

Qualify these routes serially using server-owned test configuration:

1. Force TURN on both peers and prove the selected candidate pair is relayed.
2. Keep the Sprite on TURN/UDP; restrict browser TURN URLs to TLS/443 and relay-only
   policy. Disable non-proxied WebRTC UDP in the owned browser and verify the
   setting takes effect. Inspect `relayProtocol` on the selected local candidate;
   the candidate's ordinary `protocol` can still say UDP for the allocation's
   peer-facing leg. Prove browser TLS/443 and relayed payload receipt together.
   If the browser setting is insufficient, use isolated browser-only blocking/
   observation; never change the host-wide firewall or add a proxy/listener.

For each route, show the real desktop, move a window, type and save an owned text
file, scroll, disconnect and reconnect. Check private/Origin denial, held-input
release and the expected two-process capture ownership. Preserve a full-chroma
fidelity result and actual dimensions/rate; do not label unknown latency metrics
as passes. Close the automated viewer before handing Josh the normal page link.
The trial page must disclose expiry and that this is an exploration.

The administrative `webrtc-trial.ts restore` operation restores the baseline and
verifies cleanup on completion or failed qualification. Runtime expiry stops its
own media, input and renewals even if the administrative runner disappears; it
does not restore old binaries. The recorded manifest lets the operator resume
restoration. If that step cannot complete, report the expired/unrestored candidate
explicitly instead of claiming unattended rollback.

## Validation commands

Run from the candidate workspace; these gates do not deploy it:

```sh
pnpm check:rust
pnpm test:rust:ipc
pnpm test:rust:media
pnpm exec tsc -p probes/tsconfig.json
```

The standalone relay tool also needs its own Cargo tests, fmt, Clippy and release
build. Existing ignored real-FFmpeg tests must run explicitly; skipped cases are
not passes. New credential/Task fixtures and direct-URL WebRTC checks must be
included in the executor plan's named commands. Tests cannot be claimed complete
from the existing H.264 or local-only prototype results.

After rollback, use the existing read-only native check:

```sh
pnpm exec tsx probes/rust-desktop/native-check.ts --sprite sprite-desktop-rust --suite video
```

## Qualification after the first single-viewer demo

Before any replacement decision, finish two-viewer joins and held-input handoff,
slow-peer isolation, the expanded live encoder/IPC fault matrix, long attachment,
idle/wake and the full performance/fidelity comparison. These remain obligations;
they do not all block the first time-bounded user demo. Existing regression tests
still run throughout the implementation.

## Work that this exploration does not approve

Permanent provider selection, production cutover, an SFU, self-hosted TURN,
a generic provider interface, protected-Sprite changes and broad browser support
remain outside scope. Full replacement still requires the accepted quality and
failure gates, including an agreed evidence mapping for unavailable WebCodecs
queue/reset/confidence counters. A usable demo does not claim sustained 60 distinct
images/s, hardware decoding or measured input-to-photon latency.

## Execution gates

The five slices, demo gate and operating limits are accepted. Do not request
another outline or provider approval before executing them. Cloudflare account
permissions, native TURN transport, WLR provenance and the keyframe/RTP-clock
contract remain gates that require evidence. Hand back a failed gate explicitly
rather than introducing another deployment architecture.

Implementation is authorized directly from this outline. Stop on the stated
transport, permission, source-provenance or media-contract failures. Production
replacement remains outside this authorization.
