# Rust desktop performance architecture research

This research preceded the authorized implementation trials. The investigation
itself changed no runtime or deployment. The later
[round-three report](../../probes/rust-desktop/PERFORMANCE-ROUND3.md) records the
persistent-capture experiment, new timing evidence, failed repeat acceptance and
restoration of the saved WLR baseline. The other architecture alternatives below
remain proposals. Descriptions of the current path here refer to the WLR research
baseline documented in `probes/rust-desktop/PERFORMANCE.md`.

## What needs explaining

The desktop requests 60 Hz, capture supplies about 51–52 frames/s, and the browser
presents about 49–51 updates/s. Capture callbacks often arrive 16 ms apart, with
33 ms gaps. Separate browser receipt pauses occur while capture remains regular.
These are two distinct questions: why capture misses refresh opportunities, and
where later delivery becomes uneven.

At 1824×848, one raw BGRA frame occupies 6,187,008 bytes. The pipeline transfers
about 320 MB/s of raw pixels but only about 7 Mbps of compressed media. Submission
copies take about 0.63 ms median. Complete pipe writes take about 3.76 ms median
and 7.32 ms p95. That write interval includes waiting for FFmpeg to read; it is
not a pure memory-copy benchmark. Removing the pipe cannot promise to save the
whole interval.

## Existing buffering is already mostly the right shape

`crates/streamd/src/video.rs` has three retained storage slots, one encoding frame
and at most one pending frame. New capture replaces old pending input. The worker
never reads a partially replaced frame. `wayland/capture.rs` owns a separate SHM
mapping and copies its completed contents into those retained slots.

Sunshine uses a similar separation: a reusable image pool and a latest-value
mailbox feeding each encoder. Its `event_t::raise()` replaces the previous value.
A ring that instead delivers every queued frame in order would increase frame
age when the encoder falls behind. Six queued frames at 60 FPS span about 100 ms.

Pool capacity must cover storage still owned by capture or encoding. Waiting
frames should remain limited to the freshest raw frame. These are different
bounds, even when both use the same array.

Dropping a raw frame before encoding is safe because it contains a complete
image. Dropping a decoded frame is also safe. Arbitrary compressed H.264 deltas
can depend on discarded pictures; recovery must preserve dependencies or abandon
the predictive span and resume at a suitable keyframe.

## Candidate A: persistent capture and leased SHM buffers

This is the leading modest architecture change. Retain the FFmpeg executable,
loopback RTP, gateway and viewer contract.

### Keep the capture session alive

The current wlr-screencopy path negotiates buffer constraints for every frame,
then waits for `BufferDone` before requesting the actual copy. The newer
`ext-image-copy-capture-v1` protocol advertises constraints when a session starts
and when they change. Each subsequent frame can attach a suitable buffer and
request capture without repeating that exchange.

wayvnc implements this session model. Our cursor implementation already uses the
same extension family, but main-output capture formats and behavior remain
unverified. The extension still allows only one live frame object per session.
It removes repeated constraint negotiation, not all capture synchronization.
It can also wait indefinitely for changed content after the first frame, which
makes idle behavior and fresh-client bootstrap explicit test requirements.

### Pass the captured mapping to the writer

Replace the separate capture mapping and three raw vectors with a small pool of
Wayland SHM mappings. One slot may be capturing, another may be pending, and
another may be feeding FFmpeg. Pass a lease to the completed mapping rather than
copying its pixels into another allocation. Return the slot only after the
complete raw write, or after safely abandoning that encoder generation.

This follows the reference ownership used by wayvnc/neatvnc and the capture/encode
handoff in wf-recorder. With three equally sized slots, the nominal storage falls
from four full frames to three. Resize may temporarily retain old-generation
storage; that overlap needs a separate hard bound.

Required invariants:

- Capture writes only into a free slot. The encoder sees immutable pixels.
- New pending input replaces older pending input, never encoding input.
- Cancelled capture storage is retired safely rather than reused while the
  compositor may still write it.
- Resize and restart tag every slot and returned lease with a generation.
- Old leases cannot return storage to a new generation's pool.
- Pool exhaustion drops or defers capture under a stated policy; it never grows
  an unbounded list of buffers or retired generations.

Damage history belongs to each rotated buffer. wayvnc accumulates changed regions
across all retained buffers and clears the one just refreshed. Tracking only the
last frame's damage would leave stale pixels in older slots. Full-buffer damage
is a correct initial policy and avoids this extra state until its benefit is
measured. Damage-aware capture still sends full raw frames through FFmpeg stdin.

Compare persistent and current capture with the same buffer ownership to isolate
protocol overhead. Measure copy authorization and successful socket flush, not
only the initial capture-object request. A lower copy cost alone does not prove
that capture will reach 60 FPS.

## Candidate B: a killable encoder child that consumes shared buffers

This is the strongest larger redesign if raw transfer and encoder ownership
justify the maintenance cost.

Keep two project executables. Streamd could launch itself in a private encoder
role instead of launching the stock FFmpeg CLI. That child uses FFmpeg libraries.
The capture daemon and gateway stay separate. The child remains independently
killable if codec calls or driver teardown hang.

The daemon passes memory-file descriptors once over a private Unix socket, or
inherits them into the child. Per frame it sends only a bounded descriptor:
slot, generation, dimensions, encoder configuration and capture metadata. Pixels
stay in the shared mapping. The encoder wraps the mapping in FFmpeg's reference
ownership and returns the lease only after every codec/filter reference releases
it. Acceptance by `avcodec_send_frame()` alone is insufficient.

Looking Glass demonstrates shared-memory frame storage between processes.
PipeWire demonstrates dequeue/use/return buffer ownership. This proposal combines
those ideas with our latest-pending policy and killable encoder child; neither
project is evidence for this exact design or a predicted speedup.

Important costs and limits:

- Rust/libav lifetime wrappers, ABI packaging, filter configuration and RTP
  muxing replace the current simple FFmpeg command line.
- The codec may retain several input references. Determine its required fixed
  pool capacity rather than assuming three slots always suffice.
- Descriptor and release messages need count/byte bounds, wakeups and failure
  deadlines. They carry pixel ownership, not new input-lease acknowledgements.
- Reclaim child-held slots after death only once the child has been reaped.
- Preserve the existing emitted SPS, High 4:4:4, full-range BT.709/sRGB, fidelity,
  generation and shutdown gates.
- Keeping RTP initially avoids bundling a gateway-protocol redesign into this
  alternative. Direct frame/packet metadata may justify a separate later choice.
- Pixel conversion and libx264 may still copy internally. Call this copy
  reduction, not end-to-end zero-copy.

Measure Ready-to-encoded-packet time and CPU. A shorter submission function can
hide the same work in the encoder and prove nothing about latency.

Embedding libav directly in the capture daemon is simpler than a shared-memory
child but loses independent codec termination. A hung codec thread cannot be
safely killed within a live Rust process. The child design preserves that useful
property of the current FFmpeg executable.

## Candidate C: improve delivery according to the measured stall location

Keep WebSockets unless evidence justifies replacing the transport. About 7 Mbps
of traffic alone does not prove bandwidth saturation. Large keyframes,
retransmission, proxy buffering and browser task scheduling can all produce
pause-and-batch delivery.

### If the browser event loop stalls

Move video WebSocket receipt, WebCodecs and OffscreenCanvas rendering into a
worker. The W3C WebCodecs sample demonstrates this placement, but its one-latest-
frame renderer ignores presentation timestamps. Copying that policy would
reintroduce our fixed early-frame discard bug. Preserve the bounded future queue
and select the newest frame actually due.

Keep DOM input and cursor work on the main thread. Define resize, disposal,
recording counters and clock conversion across the worker seam. Do not assume
window and worker `performance.now()` values share an epoch. A worker cannot
remove network retransmissions, and its rendering thread can still stall.

### If stale media accumulates in the sender

Bound queued age as well as bytes and count. Abandon a stale predictive span and
resume at an IDR. Prompt keyframe requests need rate limits across viewers; one
slow viewer must not repeatedly restart the shared encoder. The current stock
FFmpeg process has no implemented on-demand IDR control path.

A sender cannot retract bytes already accepted by TCP or an opaque proxy. Socket
write completion measures local acceptance, not browser delivery. Replacing a
connection can abandon a backlog, but introduces authentication, bootstrap and
reconnect costs.

### If TCP retransmission causes the pauses

WebRTC offers packet pacing, loss feedback, selective retransmission, keyframe
requests and adaptive playout. UDP avoids TCP's requirement to deliver every byte
in order before exposing later bytes. WebRTC still has codec dependencies,
congestion and recovery work; it does not guarantee timely frames.

Sprite's documented public ingress is HTTP(S). A browser WebRTC media path would
need suitable reachability or an external relay and an explicit authentication
model. TURN/TCP can restore TCP head-of-line blocking. Browser WebRTC support also
does not guarantee our High 4:4:4 profile. This is a separate transport and codec
decision, beyond the current WebSocket contract.

Increasing the presentation buffer can absorb more jitter by making frames wait
longer. A 120 ms target cannot preserve the current 60 ms target. Buffer tuning
should expose this tradeoff rather than count smoother playback as a free win.

## Other ideas to defer

- DMA-BUF and hardware encoding need a compatible device, pixel format, modifier,
  synchronization and exact color/profile support. No such device path has been
  verified in the Sprite. Downloading pixels back into raw stdin defeats much of
  the benefit. Hardware encoding alone retains the raw pipe.
- PipeWire supplies useful ownership and negotiation, but still needs a desktop
  capture producer. Adding a portal and PipeWire merely to hide our existing
  handshake adds processes without proving that any work disappeared.
- A lock-free ring does not remove the measured pixel copy or protocol exchange.
  Measure lock contention before replacing the small mutex. A producer-owned
  filling slot can shorten the critical section if contention appears.
- Removing FFmpeg `-re` is a cheap pacing experiment, not the established cause
  of the ceiling. FFmpeg 8 compares DTS with elapsed wall time; a 51 FPS live
  source declared as 60 FPS does not incur an unconditional 16.7 ms sleep per
  frame. Startup and catch-up behavior may still differ.

## Measurements that decide among these alternatives

Use a common frame identity and bounded timing-only records:

1. Verify actual application presentation and compositor presentation. Requested
   60 Hz is not proof of 60 distinct source images.
2. Record capture request, constraints arrival, actual copy authorization/flush,
   protocol Ready and local callback. Count pending replacements and pool waits.
3. Record raw write completion, RTP arrival/AU completion, metadata correlation,
   viewer dequeue and WebSocket write duration on the host clock.
4. Record browser receipt, decode completion, due time, draw and event-loop
   stalls on the browser clock. Preserve monotonic drop-cause counters.
5. Correlate frame sizes and owned-socket retransmission/send-queue observations
   with delivery gaps. Compare direct and ingress paths where an equivalent
   private route is available.

Use same-clock intervals first. Cross-host/browser one-way estimates retain
clock-offset uncertainty. Fix the first stage whose timing develops the pause.
Do not rewrite storage, encoding and transport together and then attribute the
result to one of them.

## Prior-art sources

The parent checked the current raw-slot code, the persistent-session protocol,
Sunshine's mailbox implementation and the W3C worker sample directly. Research
agents traced the other source implementations; a separate advisor challenged
ownership and failure assumptions. These are source findings, not performance
measurements from running those projects here.

- [Sunshine latest-value event](https://github.com/LizardByte/Sunshine/blob/73ccd68f3cd49a36254ce1046b2d66149cbe656d/src/thread_safe.h#L35-L64)
  and [capture pool/publication](https://github.com/LizardByte/Sunshine/blob/73ccd68f3cd49a36254ce1046b2d66149cbe656d/src/video.cpp#L1525-L1708).
- [wayvnc persistent capture](https://github.com/any1/wayvnc/blob/e0a2392c2213d4bdceacb611eb271fcac3180706/src/ext-image-copy-capture.c#L199-L249)
  and [buffer ownership/damage](https://github.com/any1/wayvnc/blob/e0a2392c2213d4bdceacb611eb271fcac3180706/src/buffer.c#L633-L700).
- [neatvnc buffer references](https://github.com/any1/neatvnc/blob/67c722dfb01076cc14faad1e62afb25be6afc2b8/src/buffer.c#L92-L143).
- [wf-recorder pool](https://github.com/ammen99/wf-recorder/blob/c5de47440e8e81c92befb696cb69819cdb3bfe8a/src/buffer-pool.hpp#L8-L107)
  and [encoder handoff](https://github.com/ammen99/wf-recorder/blob/c5de47440e8e81c92befb696cb69819cdb3bfe8a/src/main.cpp#L615-L725).
- [ext-image-copy-capture protocol](https://github.com/wayland-mirror/wayland-protocols/blob/d5aed4e4903a77aefaef03359d1ffdc0d5093456/staging/ext-image-copy-capture/ext-image-copy-capture-v1.xml).
- [PipeWire dequeue/queue interface](https://github.com/PipeWire/pipewire/blob/b0b792fa72451fd9a068c1a8f877d21d4c67cd3f/src/pipewire/stream.h#L614-L623).
- [Looking Glass shared frame ring](https://github.com/gnif/LookingGlass/blob/236efcb155f952f5d7d9fcd5891a3060ad254e68/host/src/app.c#L442-L480)
  and [full-queue backpressure](https://github.com/gnif/LookingGlass/blob/236efcb155f952f5d7d9fcd5891a3060ad254e68/host/src/app.c#L230-L249).
- [FFmpeg input-reference ownership example](https://github.com/FFmpeg/FFmpeg/blob/f93cd72dde3056c2efb39e11589745d78cd24409/doc/examples/encode_video.c#L154-L168)
  and [FFmpeg 8 readrate implementation](https://github.com/FFmpeg/FFmpeg/blob/n8.0.1/fftools/ffmpeg_demux.c#L500-L543).
- [W3C worker decode/display sample](https://github.com/w3c/webcodecs/blob/main/samples/video-decode-display/worker.js).
- [WebRTC decode scheduling](https://webrtc.googlesource.com/src/+/main/video/frame_decode_timing.cc)
  and [playout-delay policy](https://webrtc.googlesource.com/src/+/main/docs/native-code/rtp-hdrext/playout-delay/README.md).
- [TCP reliable ordered delivery](https://www.rfc-editor.org/rfc/rfc9293.html#section-2.2),
  [WebRTC media transport](https://www.rfc-editor.org/rfc/rfc8834.html),
  and [Sprite networking](https://docs.sprites.dev/concepts/networking/).
