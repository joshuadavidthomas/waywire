---
type: design-discussion
repo: sprite-desktop
change: nkspolspoyvu
revision_at_reconnaissance: b67d9106626e
date: 2026-09-07
status: accepted
source_research: repository inspection, preserved WebRTC attempts, and official Sprites/Cloudflare documentation
---

# WebRTC desktop through the existing Sprite page

## Request and review status

WebRTC is the target. Josh opens the existing private Sprite URL and uses the
actual desktop. No local proxy, command, browser extension, VPN, or manually
configured relay is part of using it. The project must account for the hosting,
networking, authentication, lifecycle and cost behind that page before another
implementation experiment.

Josh accepted this design for an exploration: keep the application on the Sprite
and use managed Cloudflare Realtime TURN to prove browser-only WebRTC delivery.
The external relay dependency is approved for that proof. Cloudflare is not a
permanent provider decision; other TURN services and self-hosting remain future
options.

The next artifact is a bounded implementation outline covering setup, operating
limits, the real-Sprite demo gates and cleanup. This approval does not authorize
production cutover, changes to protected desktops or relaxed acceptance criteria.

## What is settled

- One authenticated web page remains the entire user interface.
- WebRTC carries video into a normal browser `<video>` element.
- The Sprite still runs the desktop, captures its pixels and encodes them. Browser
  decoding/display remain unavoidable; hardware acceleration is not established.
- The two production Rust executables retain their responsibilities. The gateway
  owns networking and daemon supervision. The daemon owns Wayland and FFmpeg.
- Mouse, keyboard, composition, clipboard, cursor shapes, resize, controller
  handoff and reconnect remain required. Audio remains outside the current scope.
- Text fidelity, responsiveness and bounded resource ownership remain requirements.
  A first frame or successful connection alone is insufficient.
- Local synthetic playback is codec/player evidence only. A product-level test
  must use the private Sprite URL, actual capture and browser-delivered input.

## Current state and evidence

The current Rust portal already lives inside the Sprite. `crates/gateway/src/http.rs`
embeds `apps/stream-viewer/dist`, serves the page, and accepts `/stream` and
`/control`. `probes/rust-desktop/run.sh` starts it on port 8080 and derives the
canonical origin from Sprite metadata. Fly supplies HTTPS and private browser
login. Exact Origin checks remain an additional boundary, not authentication on
their own.

The older `apps/web` SvelteKit application and `apps/gateway` Cloudflare Worker
belong to the v0 design. They are not prerequisites for the current portal.
`SPEC.v1.md` sections 2.4 and 3 explicitly describe opening the Sprite URL without
a central application service. We should not revive the old ticket/proxy system.

The local VP9 profile-1 experiment preserved full chroma and scored about 53 dB
RGB PSNR on its owned chart. It observed about 54–55 native presentations per
second, not 60 distinct displayed images. See `probes/webrtc-exploration/README.md`.

The later Sprite experiment used a local signaling tunnel and failed before
capture started. Both ends gathered public-address candidates through STUN, but
browser ICE checks received no replies. STUN discovers an address; it does not
relay traffic. That failure does not locate a firewall or prove a general Sprite
UDP prohibition. The original WLR service was restored and checked afterward.
Evidence is in `probes/webrtc-exploration/results/sprite-network-2026-09-07/`.

### Documented network boundary

[Sprites networking documentation][sprite-network] describes two ingress paths:
the always-on Sprite URL handles HTTP(S); `sprite proxy` handles TCP on the user's
machine while its CLI runs. It documents no public raw UDP or raw TCP listener
for the Sprite. The URL is suitable for the page, signaling and control
WebSockets. It cannot be treated as a raw TURN endpoint, even on port 443.

Outbound access is documented as unrestricted by default, subject to any egress
policy. The failed attempt established a working outbound STUN exchange, not a
working TURN allocation. Reliable connectivity must not depend on undocumented
inbound reachability or successful NAT traversal on one home network.

## Recommended deployment

| Component                                     | Runs where                     | Owns                                                                                                        |
| --------------------------------------------- | ------------------------------ | ----------------------------------------------------------------------------------------------------------- |
| Browser page                                  | User's existing browser        | Native video playback, input events, session UI; no local helper                                            |
| Fly private URL router                        | Existing Sprite platform       | HTTPS, browser authentication, routing to port 8080                                                         |
| `sprite-desktop-gateway`                      | Sprite                         | Embedded page, authenticated session signaling, WebRTC peers, relay credentials, control leases, Tasks hold |
| `sprite-desktop-streamd` and its FFmpeg child | Sprite                         | Wayland capture/input, encoding, generations and child cleanup                                              |
| Cloudflare Realtime TURN                      | Managed service outside Sprite | Forwarding encrypted WebRTC packets when endpoints cannot connect directly                                  |

The browser and gateway each initiate outbound connections to the relay. Neither
needs an inbound media port on the Sprite. TURN forwards packets; it does not
capture, encode, decode or render the desktop. WebRTC's DTLS/SRTP protects media
between browser and Sprite. The provider can still see connection metadata and
traffic volume.

This keeps the application and its UI on the Sprite, but adds an external media
relay. `SPEC.v1.md` says no central service handles framebuffer traffic. Encrypted
relay traffic changes that deployment promise. Josh approved this exception for
the exploration; the long-term hosting contract remains open.

No separate SvelteKit service, ordinary HTTP Worker, SFU, or custom public relay
VM is required by this recommendation. TURN is a separate managed Cloudflare
product, not a feature automatically supplied by an existing Worker account.

### Connection and authority

1. The user opens the private Sprite URL and completes the existing Fly login.
2. The embedded page requests a desktop session on that same origin. The gateway
   admits it under bounded session/resource limits and creates a bounded Sprite
   task hold for setup.
3. The gateway obtains short-lived TURN credentials for each peer. Its own
   credentials stay on the Sprite. The browser receives only its short-lived
   credentials through an authenticated, non-cacheable response.
4. Page and gateway exchange native offer/answer descriptions and trickled ICE
   candidates through same-origin signaling. No SDP rewriting and no requirement
   to wait for every unusable network interface before connecting.
5. WebRTC selects an allowed route. As in the accepted process boundary,
   streamd's owned FFmpeg child sends codec RTP to the gateway over loopback;
   generation/frame metadata arrives separately over daemon IPC. The gateway
   validates both, maps the codec RTP into its WebRTC sender's negotiated
   payload/SSRC contract, and owns all external peer networking. The browser
   attaches the incoming track to `<video>`. It requests no camera, microphone or
   local-screen capture permission; this is a receive-only video connection.
6. The existing same-origin `/control` channel continues to carry ordered input,
   clipboard, cursor and output events. It does not pass through a prototype
   bridge or a second gateway. Control requires both a valid lease and a live
   usable media session. Loss of media revokes control and orders release-all.
7. Disconnect closes the peer, releases input, requests provider revocation of
   both peers' individual TURN credentials and drops task/session references.
   Last-viewer cleanup permits Sprite idle. Closing the peer alone does not
   invalidate credentials already delivered to a browser.

Signaling messages must bind to one admitted session and negotiation generation.
Late candidates, reconnect callbacks or provider responses cannot revive a closed
session. Offer/candidate counts, message bytes, setup time and concurrent peers
are bounded. The signaling session remains usable for ICE restart and credential
renewal; the single-use, twenty-minute prototype lifecycle is not the product.

Keep control over its existing reliable WebSocket for this change. WebRTC media
does not require moving all application messages to data channels. Moving input
would add a second protocol change without solving the network problem. Remove
the old `/stream` media path when the replacement passes; do not retain two
production video transports as a fallback.

## Infrastructure and operating requirements

### Relay account and secrets

Cloudflare requires a TURN key and its credential-generation API token. Account
setup may also require administrative API permission, but the Sprite should
receive only the runtime credential-generation authority it needs. No account,
key, billing arrangement or current secret availability was inspected in this
research.

Provision the relay once for the application, not once per browser session.
Deliver the runtime secret to a private server configuration file or approved
secret mechanism. Do not place it in binaries, browser assets, URLs, process
arguments, recordings or logs. Per-session browser credentials stay in memory and
are never written into static assets or diagnostic artifacts. Credential issuance
is authenticated and rate-limited; it cannot become a public bandwidth faucet.
A TURN credential delegates relay bandwidth, not access restricted to this one
Sprite. Do not confuse it with a desktop session credential.

[Cloudflare's credential API][cf-credentials] supports expiry and revocation.
Credentials can last up to 48 hours, but that is a provider limit, not our chosen
session lifetime. Choose an application TTL and renewal margin during the
outline. Renew before expiry and verify both peers keep using valid allocations.
Use bounded provider revocation on disconnect, including failed setup. If
revocation fails, stop renewal, record the failure without the secret, and treat
the remaining TTL as a possible abuse/billing window. Never report immediate
revocation merely because the page closed. Updating a browser configuration is
an API mechanism, not proof that a long session survives. Provider maintenance and network changes also require tested
ICE restart. Failure to renew must produce an actionable error and release input.

### Network transports and a concrete dependency gap

[Cloudflare TURN][cf-turn] supports UDP 3478, TCP 3478/80, and TLS over TCP
5349/443. Browser configuration should include the selected supported transports
and omit port 53, which browsers commonly block. TLS on 443 can help when client
UDP is blocked, but cannot promise passage through every corporate HTTP proxy.
TURN-over-TCP/TLS describes the client-to-relay connection; it does not require
RFC 6062 TCP peer allocations.

The current probe's Rust `webrtc` 0.20.5 is narrower: its
`src/peer_connection/transports/turn_relayer.rs:254-265` explicitly skips secure
TURN and non-UDP TURN URLs. It cannot currently be advertised as a native
TCP/TLS-capable TURN client.

The first proposed supported topology is therefore Sprite-to-TURN over UDP,
with browser-to-TURN over UDP or TCP/TLS. An outbound allocation is a different
path from the failed direct peer connection. It is plausible, but not yet proven
on this Sprite. If Sprite-to-provider UDP cannot sustain the session, stop and
choose a Rust implementation with the required TURN transport support. Do not
add a local proxy, an extra custom transport daemon, vendored patches or an
unreviewed library fork to work around it.

Provision only required egress destinations. A configured Sprite DNS allowlist
can also block raw candidate IPs; inspect its policy before assuming a failed
allocation is a WebRTC defect. No egress policy should be loosened implicitly.

### Suspension, wake and teardown

[Sprites keep-running documentation][sprite-tasks] calls for a Task for outbound
connections. Neither an encoder process nor successful ICE consent checks prove
that the platform will stay awake. Tasks expire and must be renewed; the maximum
single hold is one hour.

The gateway owns one renewable task while admitted attached sessions exist,
including a bounded setup/reconnect window. Renew it through `/.sprite/api.sock`
without handing an org token to the browser. Release it after the last session.
Use short renewable expiry so a gateway crash cannot leave an indefinite hold.
`bridge/tasks.go` is an existing behavioral reference: failed PUTs may still have
created a task, so cleanup must retain the release obligation.

The page reports setup, authentication, relay, media and desktop failures
separately. It must not report “connected” merely because the signaling WebSocket
opened. Automatic reconnect has a budget; explicit Disconnect cancels it. An
unavailable task hold cannot be silently reported as a stable attached session.
Idle then wake, long attachment and browser disappearance all need real checks.

### Cost and maintenance

The [current provider FAQ][cf-faq] lists 1,000 GB free before charges and $0.05/GB
for data sent from the TURN server to its TURN client, including overhead. Verify
the applicable account and billing terms before enabling it.

At a sustained 8 Mbps, one video downlink carries about 3.6 decimal GB/hour, or
about $0.18/hour at that metered rate before overhead and the free allowance.
This is a budget example, not a measured invoice. Do not double that estimate
merely because both endpoints use TURN: provider-to-peer traffic is excluded by
the documented billing model. Meter only observed TURN-server-to-TURN-client
bytes. One relayed 8 Mbps video downlink is not automatically two billable 8 Mbps
streams. Retransmissions, actual encoder overshoot and extra viewers increase
usage.
Sprite compute and any applicable platform network charges are separate.

The deployment needs usage visibility, a chosen spending threshold, credential
rotation and a response to provider outages. A billing alert is not a hard cap;
any application-enforced cutoff must release the session and explain why it
stopped. A managed relay removes VM patching and public-port maintenance, but
still creates a provider dependency.

## Native media and quality obligations

Retain native capture and the killable encoder under `streamd`; do not ship
wf-recorder plus a second capture stack alongside it. Reconcile the current
ext-capture source with the retained WLR build before making a candidate. Do not
combine this media replacement with another capture-backend comparison.

VP9 profile 1/full chroma is the evidence-backed starting codec. Preserve full
resolution, the 8 Mbps ordinary comparison setting, bounded raw ownership and
prompt keyframes. The current prototype exceeded 8 Mbps on motion; that must be
fixed or explicitly counted before claiming equal-bandwidth results. Do not
silently substitute 4:2:0, lower resolution or a different latency policy.

The gateway owns per-peer RTP/RTCP handling, retransmission/pacing resources and
feedback to the encoder. “The library handles WebRTC” does not establish bounded
queues or encoder response to congestion and keyframe requests. Slow-peer
isolation, a fresh keyframe for a new viewer, encoder generation replacement,
and existing two-viewer controller handoff remain required. No SFU is proposed
for this small deployment; larger fan-out is a separate scope decision.

Keep presentation in the native video element. Preserve frame/generation and
applied-input metadata where needed for correctness and measurement, without
rebuilding a parallel canvas player. A mapping from encoded RTP timestamps to
observed browser presentations needs evidence before it becomes an acceptance
metric.

The old acceptance harness measures custom WebCodecs queues, decoder resets,
clock confidence and canvas submissions. WebRTC does not expose the same
counters. Missing counters cannot count as zero or as a pass. Before replacement,
agree on the WebRTC-specific measurements that preserve the requirements:
full-color text (RGB PSNR at least 35 dB), ordinary full-resolution/8 Mbps samples,
60 FPS source and 60 ms presentation target, at least 45 actual presentation
updates/s, p95 gaps at most 50 ms, edge-inclusive freezes within 250 ms, and
confident lateness within 100 ms. The old queue/reset/confidence gates need an
explicit evidence mapping or a separately approved change. Physical
input-to-photon latency remains a separate real measurement, not a decode-time
or RTT claim. Sustained 60 distinct displayed images is still unproven.

## Resolved design questions

### External relay for the exploration

Use managed Cloudflare TURN for the proof. Josh explicitly approved this external
service while retaining the single-page user experience. It avoids depending on
undocumented Sprite inbound media ports. Evaluate other TURN providers or our own
server later; do not build a provider framework for this exploration.

Running coturn on the same Sprite does not solve documented ingress limits.
Running coturn on a public VM is possible, but adds VM upkeep, certificates,
public TCP/UDP listeners, relay port ranges, abuse controls and monitoring. It is
not the smaller operating burden for this project.

If every server must remain inside one Sprite, obtain a supported platform media
ingress path before committing to that deployment promise. The failed STUN test
alone cannot establish impossibility, but the current documentation does not
provide the required public listener.

### Connection policy

Provision TURN as a required capability of the exploration, not introducing
it after a direct-only release fails. First qualification must force relay use
on both peers so it cannot accidentally pass through a direct route. It must also
exercise browser TLS/TCP with browser UDP unavailable. After those pass, normal
ICE may choose a working direct route when available. These are routes within
one WebRTC implementation, not separate production media backends.

## Open implementation details

### Acceptance and operating budget

Cloudflare is approved for the exploration. The account, spending threshold,
supported browser/network matrix, credential renewal policy and replacements
for implementation-specific metrics must be settled before an executor plan. Recommend Chrome 152/Linux as the first
supported browser because it is the measured full-chroma path; other browsers
need their own codec and interaction checks. Do not claim broad browser support
from WebRTC's general availability.

## Proof required before asking Josh to test

The next user demo must already open at the real private Sprite URL in an ordinary
browser without any agent browser or local forwarding process assisting it. It
must show the actual desktop and accept mouse, keyboard and saved text input.
Before the link is offered, checks must establish:

- Forced relay on both peers, including an actual Sprite UDP TURN allocation.
- Browser TURN/TLS on port 443 with browser UDP unavailable, using the page's own
  configuration and no local network helper.
- Private authentication and exact Origin enforcement before session or relay
  credential issuance; no secrets in static assets, URLs or diagnostics.
- Full-chroma playback, saved text through browser input and basic reconnect.
- Held-input release on media loss and Disconnect, bounded credential cleanup,
  and the intended gateway/streamd ownership with no second capture stack.

That is a usable demo gate, distinct from full performance/reliability acceptance.

Before replacing the baseline, also verify unauthenticated denial and Origin
checks; provider credential expiry/revocation; quiet attached sessions across a
Task renewal; idle/wake and service restart; two-viewer handoff with held keys;
resize and local cursor behavior; clipboard/composition; slow peers; encoder and
network failure; unchanged fidelity/latency gates; and no orphaned resources.
Preserve failed attempts. Rollback restores the exact recorded baseline without
rebuilding the current ext-capture tree.

## Standards and source references

The `coding-standards` boundary guidance applies to provider credentials, native
WebRTC types and process IPC. Convert the provider response into a small internal
relay grant containing validated endpoints, expiry and secret credentials, then
into browser/native ICE configuration. Do not expose arbitrary provider payloads
or create a generic multi-provider framework.

The effects guidance applies to billable allocations, expiring Tasks and async
session ownership. Each has a named owner, a lifetime and a cleanup obligation.
The verification guidance applies to the distinction between local codec proof,
a usable Sprite demo and acceptance of the replacement.

- `SPEC.v1.md:70-163,191-248` — private single-page deployment, Tasks and auth.
- `crates/gateway/src/http.rs:119-157,217-411` — embedded routes, Origin and control.
- `crates/gateway/src/session.rs:40-115` — controller lease and ordered input.
- `probes/rust-desktop/run.sh:60-61,110-117` — canonical origin and service entry.
- `bridge/tasks.go` — renewable Task ownership and release obligations.
- `probes/rust-desktop/README.md:191-213` — existing acceptance semantics.
- `probes/webrtc-exploration/README.md` — local proof and failed Sprite attempt.
- `webrtc` 0.20.5, `src/peer_connection/transports/turn_relayer.rs:254-303` —
  UDP-only TURN in the currently selected native dependency.

## Next gate

Design accepted for the exploration. Prepare the structure outline next, with
bounded provisioning, implementation, verification and rollback. Keep the usable
demo gate separate from production replacement acceptance. No local proxy is part
of either path.

[sprite-network]: https://docs.sprites.dev/concepts/networking/
[sprite-tasks]: https://docs.sprites.dev/keeping-sprites-running/
[cf-turn]: https://developers.cloudflare.com/realtime/turn/
[cf-credentials]: https://developers.cloudflare.com/realtime/turn/generate-credentials/
[cf-faq]: https://developers.cloudflare.com/realtime/turn/faq/
