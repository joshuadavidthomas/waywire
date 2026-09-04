# WebRTC with the browser's native video player

This isolated prototype delivers full-resolution, full-chroma VP9 to an ordinary
`<video>` element. It does not use the production viewer's WebCodecs decoder,
frame queues, presentation clock, timers or canvas renderer. Browser callbacks
observe playback; they do not schedule it.

The proof works in the installed headless Chrome 152 on Linux. The text/color
chart scored about 53 dB RGB PSNR, above the existing 35 dB threshold. This is
promising evidence for deleting the custom player, not production acceptance.
Those local runs left the live Sprite and the VNC/Waymote implementations alone.

## Sprite test: blocked before video

The user then requested the actual Sprite desktop. The test was uploaded only to
`sprite-desktop-rust`, in separate `webrtc-test-*` directories. The current lockfile
resolves `webrtc`/`rtc` 0.20.5. Neither production executable was rebuilt or replaced.

- `--source desktop` adds owned wf-recorder capture piped into owned FFmpeg. Only
  wf-recorder 0.6.0 was installed; no existing packages were upgraded.
- A private Sprite CLI tunnel carries HTTP signaling and control on port 3220.
  Video attempts a separate WebRTC UDP connection using Google STUN. The desktop
  source permits ten minutes to connect and twenty minutes of media. An outer
  thirty-minute timeout bounds the remote process group.
- The test page has basic mouse/keyboard input. Its fixed control route reuses the
  existing gateway's lease and input service. It does not open the old video
  stream. This temporary test integration is not a new production contract.
- Both the Sprite and viewing host received valid UDP STUN replies. Both peers
  gathered public-address candidates. The browser nevertheless received zero
  replies to its peer connection checks; ICE remained `checking`, and DTLS never
  started. This does not identify which network device blocked the route.
- No desktop capture child started, no video played, and input was not exercised.
  The wf-recorder stdout contract, interactive latency and fidelity remain untested.
  No TURN relay was provisioned.

An initial launch check wrongly assumed `/usr/bin/timeout`; this Sprite runs the
Rust coreutils implementation. Its owned process was stopped before retrying.
A browser gathering timeout was also fixed: Chrome found a public candidate but
kept checking other interfaces. The client now submits the native description
when that candidate arrives, without rewriting SDP. The next two attempts still
failed to establish the peer connection.

All test processes, the browser and the local tunnel were stopped. Restoring the
original 1280×720 output was followed by a WLR screencopy failure and gateway exit.
The unchanged owned service definition was restarted. The native video check then
passed at `2026-09-07T15:13:34.070Z`, including healthy port 8080 and exact installed
and running WLR/gateway hashes. VNC and Waymote were not touched.

Evidence: `results/sprite-network-2026-09-07/` and
`results/sprite-ice-2026-09-07T15-07-16.388Z.json`. The next connection attempt needs
an explicitly chosen relay route. There is no working Sprite WebRTC link to hand
out yet. Do not rerun `sprite-session.ts start` expecting playback without solving
that connection problem. `sprite-session.ts stop` stops its recorded owned test;
it does not restore output size or restart the production service.

## Earlier local proof

A standalone, private Rust probe uses `webrtc`/`rtc` 0.20.0. It negotiates one
receive-only browser video track, starts one owned FFmpeg child, and forwards
loopback RTP through the library. It has no production workspace dependency or
shared native library. This third executable is a test tool, not a proposed
third production service.

The encoder uses `libvpx-vp9`, profile 1, `yuv444p`, 1824×848, 60 FPS input,
realtime mode, four encoding threads, no lookahead and a 15-frame GOP. The target
is 8 Mbps, maximum 16 Mbps. Explicit conversion uses full-range BT.709. One source
is FFmpeg's motion chart; the other is the owned text/color chart in `chart.svg`.
Neither reads another application's screen or user content.

Signaling and media bind only to loopback. There are no STUN/TURN servers,
credentials, public listeners or Sprite calls. `/offer` requires the exact local
Origin and Host, limits its body to 64 KiB, and admits one valid offer per process.
The actor has an idle-offer limit of 60 seconds, setup deadline of 10 seconds,
connect deadline of 15 seconds, and media lifetime of 90 seconds. Sticky
cancellation stops it on SIGINT, SIGTERM or a terminal connection state.

The actor owns and reaps FFmpeg. Peer close and HTTP draining have deadlines.
The runner records sampler failures without skipping the other cleanup steps.
Forced cleanup can signal only its tracked server and a child whose PID/start
identity and parent it verified. An incomplete HTTP-body test exercises the
server's bounded shutdown.

## Codec finding

Chrome's actual receiver capabilities included H.264 High 4:4:4 (`f4001f`), which
is more promising than assuming every browser WebRTC path requires 4:2:0.
However, the advertised H.264 level was 3.1. Our resolution/rate requires at least
level 4.2 by macroblock limits. Requesting `f4002a` through `setCodecPreferences`
returned `InvalidModificationError`. The prototype does not rewrite SDP or label
an oversized stream as level 3.1.

The same browser advertised VP9 `profile-id=1`. This path preserves full color
detail without the H.264 level mismatch, and actual playback negotiated it.
This is why the working probe uses VP9, not a claim that H.264 can never work.
Neither `decoderImplementation` nor `powerEfficientDecoder` was exposed in these
Chrome statistics. Hardware decoding remains unverified.

References: [RFC 6184 offer/answer rules](https://www.rfc-editor.org/rfc/rfc6184.html#section-8.2.2),
[`setCodecPreferences`](https://w3c.github.io/webrtc-pc/#dom-rtcrtptransceiver-setcodecpreferences),
[Chromium H.264 capability construction](https://github.com/webrtc-mirror/webrtc/blob/main/modules/video_coding/codecs/h264/h264.cc).

## Observations

Each measurement lasted 30 seconds after playback warmup. CPU percentages below
use **one logical core as 100%**. The browser-driver tree and server/FFmpeg tree
were sampled separately with the existing host sampler. Both ran on this same
16-core host, so this is not a matched comparison with the deployed server.

| Run               | Native presentation rate | Browser CPU median | Sender + FFmpeg CPU median | Received payload rate |     RGB PSNR |
| ----------------- | -----------------------: | -----------------: | -------------------------: | --------------------: | -----------: |
| Motion, 10:45     |                  55.40/s |             86.78% |                    149.09% |       about 9.35 Mbps |   Not paired |
| Text chart, 10:58 |                  54.57/s |             88.14% |                    164.25% |       about 8.01 Mbps | 53.007573 dB |

Chrome's final decode-rate sample was 60 FPS in both. That is **not** 60 displayed
or distinct source images per second. The presentation rate above uses the change
in `requestVideoFrameCallback`'s `presentedFrames` between its first and last
observations. These were about 54–55 presentations per second. Callback counts
were slightly lower. No source-frame counter ran in this prototype.

Both runs reported zero RTP packet loss, NACKs and WebRTC freeze events. The
library's freeze statistic is not the existing edge-inclusive 250 ms test.
There is no claim that the normal zero-reset, clock-confidence, lateness or
60 ms presentation-target gates passed: those production measurements have not
been ported to this native player. Motion's actual payload exceeded the 8 Mbps
target, so it is not an equal-bandwidth performance win either.

The fidelity check rasterizes the owned SVG to a PNG, streams it, then reads the
video element's pixels once after measurement. RGB PSNR compares that decoded PNG
with the exact source PNG. There is no per-frame pixel readback during measurement.
This new chart is not the older paused `testsrc2` chart, and its 53 dB score must
not be presented as an improvement over the older 35–36 dB scores.

### Preserved attempts

All timestamps below are UTC on 2026-09-07, with files under `results/`.

- `motion-2026-09-07T10-33-08.044Z`: setup failed because FFmpeg requires
  `-strict experimental` for its VP9 RTP packetizer. Child exited and was reaped.
- `motion-2026-09-07T10-34-15.008Z`: ICE connected, but video did not play. The
  loopback RTP payload number was not mapped to the negotiated browser number.
  This was fixed using the library's native sender parameters, not SDP rewriting.
- `motion-2026-09-07T10-37-43.782Z`: first working motion run.
- `chart-2026-09-07T10-38-48.207Z`: 53.046428 dB, but 513 reported lost packets.
- `chart-2026-09-07T10-43-33.351Z`: after requesting the existing gateway's 4 MiB
  receive-buffer size, playback reported no loss; PSNR 53.195027 dB. The log records
  the actual kernel capacity. Added ingress counters showed no gaps in this run;
  those counters were absent from the earlier loss run and cannot locate its loss
  retrospectively.
- `motion-2026-09-07T10-45-35.933Z` and `chart-2026-09-07T10-46-21.382Z`: both
  browser and server CPU sampled, no reported packet loss. The chart was 53.077923 dB.
- `chart-2026-09-07T10-58-25.729Z`: rechecked after review fixed shutdown bounds and
  failure-safe runner cleanup. PSNR 53.007573 dB; the tracked encoder was absent,
  browser closed, server exited normally, and port 3220 was empty.

Each result records its own binary hash. Later formatting and cleanup changes
were not retroactively assigned to earlier recordings.

## Run locally

From the repository root, with port 3220 free:

```sh
pnpm exec tsc -p probes/webrtc-exploration/tsconfig.json
cargo build --release --manifest-path probes/webrtc-exploration/Cargo.toml
pnpm exec tsx probes/webrtc-exploration/run.ts motion
pnpm exec tsx probes/webrtc-exploration/run.ts chart
```

The runner uses its own `rust-webrtc-exploration` agent-browser session, creates a
new result directory, and closes its browser/server afterward. Each server accepts
one session only. `PLAYBACK_OBSERVED` means video was observed, not that full desktop
acceptance passed. Use the runner serially; never start both sources concurrently.

For manual local viewing:

```sh
cd probes/webrtc-exploration
target/release/webrtc-exploration --source motion
```

Open `http://127.0.0.1:3220` and select Connect. Stop ends the single session.
The process also stops after its bounded idle/media lifetime.

Validation performed:

```sh
pnpm exec tsc -p probes/webrtc-exploration/tsconfig.json
pnpm exec tsc -p probes/tsconfig.json
cargo fmt --manifest-path probes/webrtc-exploration/Cargo.toml -- --check
cargo test --manifest-path probes/webrtc-exploration/Cargo.toml
cargo clippy --manifest-path probes/webrtc-exploration/Cargo.toml --all-targets -- -D warnings
pnpm exec tsx --test probes/webrtc-exploration/shutdown.test.ts
```

Thirteen Rust tests and the real unfinished-HTTP-body shutdown test passed after
the Sprite changes. Strict TypeScript, Clippy and the release build also passed.
Local media recordings were not repeated after the network/control additions.
Production media/IPC tests were not rerun because production source was unchanged.

## What this permits us to simplify

The small `connect()` function in `client.ts` supplies an offer, receives an answer,
and attaches the incoming track to the video element. Most of that file records
observations or maps basic mouse and keyboard input for this experiment. There is no `VideoDecoder`, pending-frame array,
keyframe/reset policy, source-clock conversion or presentation scheduler.

A production replacement could delete those duties from the existing SDK. Input,
clipboard, resize, cursor handling, lease ownership, authentication and reconnect
behavior still need to remain. The existing 3500-line SDK is not all removable
video code. The prototype's signaling and lifecycle code is not a finished
production gateway either.

The next unresolved question is whether the same native player works with the
real Sprite capture path and a reachable media connection. That requires verifying
ICE/UDP connectivity or an explicitly chosen relay, then testing actual interactive
latency and the existing quality/failure gates. Local loopback success answers
none of those network or interaction questions. No production replacement or
relaxed acceptance rule has been approved here.
