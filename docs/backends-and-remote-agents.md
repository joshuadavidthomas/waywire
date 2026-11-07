# Sketch: other desktops and remote agents

Status: a design sketch for discussion. It is not on the roadmap and nothing
here is planned or built. Today waywire is one gateway and one compositor, and
[ROADMAP.md](../ROADMAP.md) puts local hardware out of scope.

This sketch explores two linked ideas:

1. **Backends.** The gateway streams a desktop it does not own, such as GNOME,
   KDE, Sway, Hyprland or gamescope on a Steam Deck, on a real machine.
2. **A remote agent API.** The gateway carries the desktop's agent interface
   (receipts, window list, settling, screenshots) over the network, so an
   agent somewhere else can drive the machine without reading video through a
   browser.

## What exists today

The gateway does not link the compositor. It starts the program named by
`--compositor` with flags such as `--frame-rate`, `--bitrate`, `--rtp-port`,
`--xkb-layout` and `--resolution`, then:

- sends commands on the program's stdin and reads events from its stdout, in
  the pipe vocabulary in `crates/waywire-protocol/src/pipe.rs`;
- receives H.264 either as `EncodedFrame` events (OpenH264) or as RTP on the
  local port (FFmpeg).

Pipe commands are input (absolute and relative pointer, buttons, scroll, keys,
text), clipboard, resize, quality, video reset and the input lease. Pipe events
are frames, cursor shape, image, position and clicks, clipboard, resize
results, agent input state and input receipts.

There is no handshake: the gateway assumes the other end is waywire's
compositor and supports every message.

The agent interface in [input-delivery.md](input-delivery.md) lives on the
desktop's session bus. Only receipts for a viewer's own input cross the
gateway, as `input-receipt` control messages. The window list, focus and map
events and `WaitSettled` are reachable only from processes on the machine.
An on-demand screenshot API is not built.

## Idea 1: backends

A backend is any program that speaks the pipe protocol. The compositor is one.
A capture backend would be another: it reads frames from a running desktop and
injects input into it.

| Concern  | Compositor backend (today) | Capture backend                                                                                 |
| -------- | -------------------------- | ----------------------------------------------------------------------------------------------- |
| Video    | renders its own scene      | PipeWire stream from the ScreenCast portal, or gamescope's PipeWire node                        |
| Encode   | OpenH264 or libx264        | VAAPI or another hardware encoder, ideally importing dmabuf frames                              |
| Input    | its own seat               | RemoteDesktop portal with libei; uinput where no portal exists (gamescope, gamepads)            |
| Audio    | private PipeWire daemon    | the host's PipeWire sink monitor                                                                |
| Cursor   | shape or image, position   | PipeWire cursor metadata when the stream provides it, otherwise drawn into the video            |
| Resize   | changes its output         | usually refused: the host owns the mode                                                         |
| Receipts | yes                        | no: only the compositor knows where input landed                                                |
| Windows  | yes                        | partly: foreign-toplevel on wlroots compositors, KWin scripting; positions are not always known |
| Settle   | from damage                | approximated from the PipeWire stream's damage or frame cadence                                 |

### Capabilities

With more than one backend, the gateway must stop assuming every message works.
A first pipe message from the backend would list what it supports, for example
`receipts`, `windows`, `settle`, `screenshot`, `resize`, `cursor-shape`,
`clipboard`, `gamepad`. The gateway hides or refuses what is missing, and the
viewer and SDK learn the same list from the control channel.

This comes first, before any capture code. Without it, every new feature has to
guess which backend it runs on.

### Gamepads

Neither backend has gamepad input today. It would be a new command in both
protocols: the viewer reads the browser's Gamepad API, and the backend creates
a virtual controller through uinput. It matters mostly for the gamescope case.

### Example: Steam Deck

Run the capture backend on the Deck and open the viewer on another machine.
Desktop Mode could use KWin's ScreenCast and RemoteDesktop portals. Game Mode
could read gamescope's PipeWire node and inject through virtual input devices.
H.264 through VA-API is the candidate encoder for the Deck's AMD GPU.
These are proposed paths, not a tested Deck configuration. The source research
below confirms gamescope's export mechanism, not the permissions, format
negotiation or latency on the SteamOS version installed on a particular Deck.

## Idea 2: a remote agent API

Today an agent on another machine can only drive waywire the way a person
does: open the viewer in a browser, read the video and send input. That loses
most of what owning the compositor provides. It reads compressed frames, it
cannot see the window list, and it has to guess when the screen has settled.

Instead, the gateway would carry the session-bus interface over its control
channel:

- `GetWindows` and `WindowsChanged`, `FocusChanged`, `WindowMapped`
- `WaitSettled`, with the same options and reply
- receipts, as viewer receipts already do
- screenshots on demand, of the screen or one window, lossless, typically
  taken when `WaitSettled` replies (planned in the roadmap, not built)
- possibly the AT-SPI accessibility tree, for text and widgets

Keep the names, keys and value strings from
[input-delivery.md](input-delivery.md) so one driver works locally over D-Bus
and remotely through the gateway.

### No video unless someone watches

A vision model needs a still image when the screen has settled, not a 60 fps
stream. An agent connection would carry events and on-demand screenshots, and
the backend would encode video only while a viewer is connected. That saves
CPU and bandwidth on the machine.

### Who connects

An agent then talks to the gateway directly, over WebSocket or through an MCP
server built on it, without a browser in the middle. The browser viewer remains
the person's window into the same session. The existing input arbitration
applies to both: a browser takeover pauses the agent and handback resumes it.
The SDK can expose the same events, so an agent that does run in a browser
gets them too.

On a capture backend, the API offers what the capability list says: AT-SPI and
a partial window list are possible. An injection acknowledgement is possible,
but it must not claim the compositor's dispatch-receipt guarantee.

## Prior art

Checked November 6, 2026. Earlier dogfood threads already measured Selkies
and cua-driver against waywire. The additional projects below were inspected
at the linked commits. This is source research, not a new latency benchmark or
a successful integration test. Upstream performance claims are not measurements
of waywire or of a Steam Deck.

### Already measured

- **Selkies.** The dogfood runs compared waywire with Selkies in X11 and in
  Wayland mode (labwc nested on pixelflux). Selkies' viewer latency led to the
  playout and draw-on-decode fixes; waywire's default striped encoder now
  measures about 14 ms p50 against Selkies' 16 ms. Selkies already has the
  backend split this sketch describes: it starts its own headless compositor
  or captures an external one, and it turns browser gamepads into uinput
  devices. Its Computer Use API has no window list, receipts or settle signal,
  and agent drivers on it work from pixels.
- **cua-driver.** It is a driver for waywire, not a competing desktop: it
  drives the compositor through screencopy, AT-SPI and the portals, the
  roadmap lists it as a Pass 1 target, and `just acceptance cua-driver` runs
  the takeover scenario with it. The proposed Pass 3 builds receipts, the
  window list and settling into the drivers agents use, cua-driver after
  computer-use-linux. A remote agent API would let the same drivers reach a
  waywire machine over the network instead of only from inside it.

The historical evidence is in the
[waywire agent dogfood](https://ampcode.com/threads/T-01a1963d-5d64-704f-8dc0-acb48e0c9e05),
[Selkies Wayland dogfood](https://ampcode.com/threads/T-01a1974f-d4e5-7626-9b67-b4ebe9b8fbec)
and [viewer latency breakdown](https://ampcode.com/threads/T-01a1974f-cb2b-717e-9b50-0978fb8688e0).
Those results describe those builds and fixtures, not current upstream versions.

### Cua Spaces already has a remote agent API

Cua Spaces is separate from cua-driver, but delegates desktop actions to it.
Its guest daemon, `cua-spacesd`, serves gRPC and gRPC-Web services for screenshots,
input, windows, accessibility, streams and participant presence. It also exposes
HTTP MCP. `WindowsService` has both a list and a watch, with opaque window ids
and epochs. `AccessibilityService` uses snapshot-scoped element ids.
These are implemented services, not only a product description.

The reference Linux image is Ubuntu with XFCE on Xvfb, session D-Bus, AT-SPI and
PipeWire. The image can supply a container root filesystem or a VM disk.
Spaces can also attach to a daemon on an existing machine. This is not one
required compositor architecture.

Linux support is desktop-specific. The inspected Hyprland backend uses
Hyprland IPC for windows and geometry, plus persistent `wlr-screencopy` for
damage-driven shared-memory capture, capped at 30 fps. `grim` is a 10 fps
fallback, not the primary implementation despite the shorter README description.
Its window capture reads a composited output region, so an occluding window
appears in that capture. Linux encoding defaults to OpenH264. The VA-API module
probes hardware but explicitly skips selection because its encode session is
not implemented in this build. This is not evidence of a GPU game-streaming
path.

RCDP wire v2 separates gRPC session setup from H.264 and Opus media. Media uses
either a ticketed WebSocket or direct QUIC with a pinned certificate. The
HTML5 viewer uses the WebSocket path and WebCodecs. Native QUIC support does
not mean browsers can connect to arbitrary QUIC sockets. The daemon normally
requires a token for a non-loopback bind, with an explicit restricted bootstrap
exception. Short-lived media tickets avoid putting that root credential on
media connections.

The `DeliveryReport` fields describe foreground or background delivery, focus
change, pointer movement and a detail string. They do not establish the same
contract as waywire's compositor-generated receipt naming the actual dispatch
target. RCDP also has input acknowledgements and action/frame correlation.
Those are relevant prior art, but neither their names nor a successful injection
prove that an application consumed an event. Waywire's own receipt likewise
does not prove application handling. No equivalent of waywire's serial-scoped,
damage-based `WaitSettled` was found in the inspected `cua.env.v1` contracts.
That is a bounded finding, not a claim about every Cua tool or future version.

Could a Space use waywire? It is a plausible custom Linux guest integration,
not an existing adapter. The fact that cua-driver already drives waywire helps
with input. It does not make Spaces' capture, window enumeration or display
discovery understand waywire. Those need an adapter and a real test.

Sources at `ec4d973dbd7eb04d85cde0f157b43c05e1204948`:

- [Daemon architecture, capabilities and authentication](https://github.com/trycua/cua/blob/ec4d973dbd7eb04d85cde0f157b43c05e1204948/libs/cua-spacesd/README.md)
- [RCDP media contract](https://github.com/trycua/cua/blob/ec4d973dbd7eb04d85cde0f157b43c05e1204948/libs/cua/proto/MEDIA.md)
- [Window service implementation](https://github.com/trycua/cua/blob/ec4d973dbd7eb04d85cde0f157b43c05e1204948/libs/cua-spacesd/crates/cua-spacesd-desktop/src/grpc/windows.rs),
  [computer service implementation](https://github.com/trycua/cua/blob/ec4d973dbd7eb04d85cde0f157b43c05e1204948/libs/cua-spacesd/crates/cua-spacesd-desktop/src/grpc/computer.rs)
  and [delivery contract](https://github.com/trycua/cua/blob/ec4d973dbd7eb04d85cde0f157b43c05e1204948/libs/cua/proto/cua/env/v1/common.proto)
- [Hyprland capture](https://github.com/trycua/cua/blob/ec4d973dbd7eb04d85cde0f157b43c05e1204948/libs/cua-spacesd/crates/cua-spacesd-desktop/src/linux_wayland.rs)
  and [VA-API probe limitation](https://github.com/trycua/cua/blob/ec4d973dbd7eb04d85cde0f157b43c05e1204948/libs/cua/crates/cua-media-codec/src/backends/vaapi.rs)
- [Licensing](https://github.com/trycua/cua/blob/ec4d973dbd7eb04d85cde0f157b43c05e1204948/legal/LICENSING.md).
  The SDK and driver are MIT, but Spaces, its daemon and streaming components
  are FSL-1.1-MIT, source-available with a competing-use restriction until the
  release's future MIT conversion. Do not treat the whole repository as MIT.

### Wolf is useful compositor and encoder prior art

Wolf's `gst-wayland-display` is a Smithay compositor exposed as a GStreamer
source. It uses GLES, including a software-device option, rather than Pixman.
Its output paths include CPU-readback raw frames, GBM dmabuf and optional
CUDA import. It advertises linux-dmabuf feedback and conditionally explicit
synchronization. This is a concrete example of keeping rendered frames on the
GPU through hardware encoding.

Wolf separates the producer from client encoders with GStreamer inter-pipes
and a one-buffer dropping queue. Encoder choices include VA encoders and
NVENC with low-latency settings. Sessions have separate desktop and audio state.
Wolf normally starts its own headless desktop, not a capture client attached to
the Deck's existing gamescope session. Its images support nested Sway and
gamescope, with Sway now the default. Audio uses PulseAudio interfaces, which
can also connect to a compatible PipeWire server.

Wolf speaks the Moonlight protocol, not a browser-native transport. Its
`inputtino` library provides virtual controllers, with uinput and uhid paths
for different devices, including DualSense features. Browser Gamepad API
support must be assessed separately. A capable host emulator cannot recover
gyro or touch data that a browser does not expose.

Useful ideas for waywire are dmabuf-aware output ownership, bounded frame
queues and controller emulation. Adopting Wolf's whole GStreamer or Moonlight
stack is a separate architectural choice, not a prerequisite for those ideas.
Passing raw dmabuf frames between processes would require file-descriptor
passing and synchronization, not the current encoded-video stdin/stdout
protocol. Hardware encoding also needs a real render device, so this research
does not justify removing waywire's CPU path.

Sources:

- [gst-wayland-display renderer](https://github.com/games-on-whales/gst-wayland-display/blob/016b4fc66d62b6a34c37169ecf6d511eef140d7c/wayland-display-core/src/utils/renderer/mod.rs)
  and [buffer allocation](https://github.com/games-on-whales/gst-wayland-display/blob/016b4fc66d62b6a34c37169ecf6d511eef140d7c/wayland-display-core/src/utils/allocator/mod.rs)
- [Wolf encoder pipelines](https://github.com/games-on-whales/wolf/blob/adf78d6061b872673c7d513bca6b683daec55466/src/moonlight-server/state/default/config.include.toml)
  and [desktop configuration](https://github.com/games-on-whales/wolf/blob/adf78d6061b872673c7d513bca6b683daec55466/docs/modules/user/pages/configuration.adoc)
- The earlier thread's [inputtino research](https://ampcode.com/threads/T-01a1b2b0-6267-7536-a0d2-01fa2f2260b9)
  inspected `d28ec79eb63324e68d73a7de22bcb5ff0a6f6bf8`, newer than Wolf's pinned
  dependency. Do not assume all of that version's features ship in Wolf.
- The projects have MIT top-level licenses, but gst-wayland-display's
  [presentation handler](https://github.com/games-on-whales/gst-wayland-display/blob/016b4fc66d62b6a34c37169ecf6d511eef140d7c/wayland-display-core/src/wayland/handlers/presentation.rs)
  and [wl_drm protocol module](https://github.com/games-on-whales/gst-wayland-display/blob/016b4fc66d62b6a34c37169ecf6d511eef140d7c/wayland-display-core/src/wayland/protocols/wl_drm.rs)
  carry GPL-3.0-only headers. Check individual files before reusing code.

### Sunshine separates capture from input

Sunshine's Linux source has X11, wlroots screencopy, DRM/KMS, portal and direct
KWin capture paths. The direct KWin path obtains a PipeWire node through
`zkde_screencast_unstable_v1` and documents its desktop-file permission check.
The portal path persists a restore token. The shared PipeWire consumer supports
dmabuf negotiation and hardware-encoder paths, with CPU fallbacks.

This is more than one universal Wayland backend. KMS capture can require
`CAP_SYS_ADMIN`, portals have consent and persistence semantics, and direct
KWin capture is compositor-specific. Capture permission does not automatically
provide input permission. Current Sunshine uses `libvirtualhid` for Linux
input, so the historical description that it always uses inputtino is stale.
Do not infer portal/libei input support merely from its portal capture code.

Sources at `010f9564e521cb4f3459795187fe87d0719583bf`:

- [Backend selection](https://github.com/LizardByte/Sunshine/blob/010f9564e521cb4f3459795187fe87d0719583bf/src/platform/linux/misc.cpp),
  [KWin capture](https://github.com/LizardByte/Sunshine/blob/010f9564e521cb4f3459795187fe87d0719583bf/src/platform/linux/kwingrab.cpp)
  and [portal capture](https://github.com/LizardByte/Sunshine/blob/010f9564e521cb4f3459795187fe87d0719583bf/src/platform/linux/portalgrab.cpp)
- [PipeWire consumer](https://github.com/LizardByte/Sunshine/blob/010f9564e521cb4f3459795187fe87d0719583bf/src/platform/linux/pipewire.cpp)
  and [Linux input implementation](https://github.com/LizardByte/Sunshine/blob/010f9564e521cb4f3459795187fe87d0719583bf/src/platform/linux/input/virtualhid.cpp).
  Sunshine's top-level license is GPL-3.0.

### Gamescope exports a real PipeWire source

Upstream gamescope initializes a PipeWire `Video/Source` when built with
PipeWire support. It logs the stream's node id and exposes it through the
`gamescope_pipewire` Wayland global. Its export negotiates BGRx or NV12,
dmabuf or MemFd buffers, with a linear, single-plane restriction in the
inspected dmabuf allocation path.

The exporter requests header and requested-size metadata, not `SPA_META_Cursor`.
The `paint_pipewire` function builds its own capture composition rather than
promising a byte-for-byte copy of the physical scanout. A capture backend must
check cursor behavior, overlays and focus selection instead of assuming generic
portal cursor metadata or complete screen mirroring.

There is no Steam-client credential exchange in the inspected node publication
code. A third-party consumer is therefore plausible, but it still needs access
to the right Wayland and PipeWire sessions. Source inspection does not prove
that SteamOS exposes those sessions to the intended process or that its shipped
gamescope has these exact features.

Sources at `309eee74f967d458cf71836619132010f88ee796`:
[PipeWire exporter](https://github.com/ValveSoftware/gamescope/blob/309eee74f967d458cf71836619132010f88ee796/src/pipewire.cpp),
[node publication](https://github.com/ValveSoftware/gamescope/blob/309eee74f967d458cf71836619132010f88ee796/src/wlserver.cpp)
and [capture composition](https://github.com/ValveSoftware/gamescope/blob/309eee74f967d458cf71836619132010f88ee796/src/steamcompmgr.cpp).

### Two different Moonlight browser projects

**moonlight-web-stream**, by MrCreativ3001, is a Rust web server that bridges
Sunshine traffic into browser WebRTC. Its current implementation sends video
and audio as RTP tracks, with a control data channel. It also has a WebSocket
transport and browser decoder alternatives. Gamepad and keyboard-lock support
depend on browser APIs and a secure context. It supports TURN configuration
for networks where direct WebRTC fails. This project already informed waywire's
viewer fallback work.

Sources at `7b9821430e07087f459db3db9aac550e1e9c9bff`:
[README](https://github.com/MrCreativ3001/moonlight-web-stream/blob/7b9821430e07087f459db3db9aac550e1e9c9bff/README.md)
and [WebRTC video implementation](https://github.com/MrCreativ3001/moonlight-web-stream/blob/7b9821430e07087f459db3db9aac550e1e9c9bff/src/api/stream/webrtc/video.rs).
Its Cargo manifest declares GPL-3.0-or-later.

**Moonlight-Web**, by linckosz, now includes its own capture/encode host as well
as a GameStream bridge. Its documented browser transports include WebRTC
DataChannel video decoded with WebCodecs, WebRTC media-track video, and a
WebSocket fallback. The DataChannel implementation uses an ordered video
channel with a packet lifetime bound. WebRTC audio uses a real Opus media
track. It also implements pointer lock, browser gamepads and rumble feedback.
Its own documentation describes no TURN relay, unlike moonlight-web-stream's
configurable TURN support. That difference matters outside a reachable LAN.

Sources at `90897076f46ae416536bccc03da0814e24766aeb`:
[README](https://github.com/linckosz/moonlight-web/blob/90897076f46ae416536bccc03da0814e24766aeb/README.md),
[transport design](https://github.com/linckosz/moonlight-web/blob/90897076f46ae416536bccc03da0814e24766aeb/docs/wiki/05-Streaming-and-Transports.md)
and [DataChannel implementation](https://github.com/linckosz/moonlight-web/blob/90897076f46ae416536bccc03da0814e24766aeb/backend/src/streaming/DataChannelRelay.cpp).
The project declares GPL-3.0. Its latency figures are upstream reports, not an
independent comparison with waywire.

Both projects establish that a browser client for a gaming host is feasible.
Neither establishes that waywire must replace its transport before trying a
capture backend. Compare frame age and input-to-display latency under the same
loss, Wi-Fi and decoder conditions before choosing.

### GNOME and KDE show two levels of integration

GNOME Remote Desktop uses Mutter's remote-desktop and screencast APIs,
PipeWire for pixels and libei for input. It is not a generic portal client.
Its PipeWire receiver handles dmabuf and cursor metadata. It supports sharing
an active session, a single-user headless session, and GDM-integrated remote
login through RDP. Its implementation includes virtual-monitor capture,
clipboard, audio playback and audio input, plus VA-API encoding code.
Describing it as video and input only misses substantial existing behavior.

Sources at `ba1b66d554c5c3edddac5882ecba38ec9c6144f5`:
[architecture](https://github.com/GNOME/gnome-remote-desktop/blob/ba1b66d554c5c3edddac5882ecba38ec9c6144f5/README.md),
[session modes and credentials](https://github.com/GNOME/gnome-remote-desktop/blob/ba1b66d554c5c3edddac5882ecba38ec9c6144f5/docs/configuration.md),
[Mutter session management](https://github.com/GNOME/gnome-remote-desktop/blob/ba1b66d554c5c3edddac5882ecba38ec9c6144f5/src/grd-session.c)
and [PipeWire receiver](https://github.com/GNOME/gnome-remote-desktop/blob/ba1b66d554c5c3edddac5882ecba38ec9c6144f5/src/grd-rdp-pipewire-stream.c).
The project declares GPL-2.0-or-later.

KRdp has both a generic portal session and a direct Plasma session. The portal
session persists its restore token, opens the permitted PipeWire remote and
calls `ConnectToEIS`. The Plasma session uses KWin screencast streams and
`org.kde.KWin.EIS.RemoteDesktop` directly. It can request a virtual monitor.
H.264 encoding delegates to KPipeWire, and clipboard support already exists.
The README still requires a logged-in user session. Creating a virtual display
is not equivalent to GNOME's system-level remote-login service.

Sources at `e7a327d0d9f8135d2f8038dd1f3a5708b173aace`:
[README](https://github.com/KDE/krdp/blob/e7a327d0d9f8135d2f8038dd1f3a5708b173aace/README.md),
[portal session](https://github.com/KDE/krdp/blob/e7a327d0d9f8135d2f8038dd1f3a5708b173aace/src/PortalSession.cpp),
[Plasma session](https://github.com/KDE/krdp/blob/e7a327d0d9f8135d2f8038dd1f3a5708b173aace/src/PlasmaScreencastV1Session.cpp)
and [video encoding](https://github.com/KDE/krdp/blob/e7a327d0d9f8135d2f8038dd1f3a5708b173aace/src/VideoStreamSurface.cpp).
Licenses vary by file, including LGPL session code and GPL-2.0-or-later video
code. The separately inspected
[westers fork](https://github.com/westers/krdp/blob/c2d1685cd479067ac7d8c3bd3a86400a381e2701/README.md)
has broader Console/Virtual session and policy work. Its README distinguishes
installed behavior from incomplete acceptance and migration work. This was a
README review, not a source audit or performance validation of that fork.

For waywire, the lesson is to separate capture, input and session lifecycle.
A portal-backed capture implementation need not implement RDP or adopt a
desktop's private APIs. Private integration can provide more functionality,
but carries compositor-specific permission and compatibility work.

### What waywire would add

The dogfood results showed that a driver using waywire's receipts, window list
and settling passed 26 of 26 runs in those fixtures. That is evidence for the
interfaces, not for global uniqueness. Cua Spaces already combines structured
desktop information, remote actions and streaming.

Waywire's narrower distinction is that its own compositor can report the
actual input dispatch target and order that receipt with focus, window and
damage-based settling events. A remote API could preserve those semantics.
A capture backend cannot promise the same evidence merely because its input
injection call succeeded. Neither a dispatch receipt nor a quiet screen proves
that an application completed the intended action.

The most useful next experiment is to consume gamescope's PipeWire export on
an actual Deck. Check node discovery, permitted access, buffer formats,
encoder import, cursor and overlay behavior, and input independently. Measure
frame age before changing transport. Keep that gaming experiment separate
from deciding the remote agent API's names and guarantees. None of these
research findings commits either direction to the roadmap.

## Security

The gateway has no authentication, and today it relies on a proxy in front of
it. That is tolerable for a sandbox. It is not for a real machine, and the
agent API makes the stakes higher: window titles, screenshots and the
accessibility tree are the machine's contents in readable form.

Before either idea runs on a real machine:

- authentication in or in front of the gateway, required, not optional;
- separate scopes for watching, controlling and the agent API;
- a visible indicator on the desktop while anyone is connected.

## Open questions

- How much of the gateway assumes the compositor's replies arrive (resize
  results, keyframe readiness, cursor events)? That decides how big the
  capability change is.
- Does gamescope's PipeWire node give dmabuf frames and cursor metadata
  usable from outside Steam? Not yet checked on a Deck.
- Is WebSocket over TCP good enough for interactive play on Wi-Fi, or does
  video need WebRTC or WebTransport? Measure frame age first.
- Should the agent API be a new WebSocket endpoint, part of the viewer control
  channel, or MCP first?
- Do capture backends live in this repository as new crates? Probably yes; the
  gateway, protocol, encoder and viewer are shared.
- Acceptance scenarios for a capture backend could run against a nested Sway
  or KWin in an orb. The compositor's existing checks stay as they are.
