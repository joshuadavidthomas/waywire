# waywire

waywire is an experimental remote desktop built around a headless Rust compositor.
[Smithay](https://github.com/Smithay/smithay) manages Wayland applications and a
single seat; Pixman renders on the CPU, and FFmpeg encodes H.264. A Rust gateway
relays video, input, and clipboard messages to a browser using WebCodecs.
No wlroots compositor, desktop environment, GPU, or physical display is required.
The project began as a port of [waymote](https://github.com/rockorager/waymote).

## Why own the compositor

The goal is a real, headless desktop for agents on your own machines, VMs, or
microVMs. Agents drive it through computer-use drivers, and a person watches or
takes over from a browser tab. It should feel like the sandbox computers in
hosted agent products: a minimal desktop with overlapping windows, not a
single-app stream.

Waymote pairs labwc with a separate streaming process. They negotiate frames,
cursor, clipboard, output size, and input through Wayland protocols. Everything
the gateway needs belongs to the compositor, so waywire owns it directly:

- Frames render straight into the encoder, and the cursor is reported by shape
  or image and position instead of being captured.
- One seat serves the browser and agent drivers. XTest- and libei-based drivers
  arrive through the EIS socket instead of stopping inside Xwayland.
- Agent operations and desktop controls should share one implementation. A
  titlebar click and an automation command perform the same window operation.

Agent queries are planned but not built: a window list, a settled-frame signal,
per-window capture, and screenshots on demand. Input arbitration is shared with
agent sources: a browser takeover releases and pauses agent input, and handback
resumes it. See [ROADMAP.md](ROADMAP.md).

The compositor owns the windows, output, and cursor directly. Cursor images are
not baked into the video: the controlling browser uses its native cursor, while
observers and pointer-locked clients show a positioned image or fallback arrow.
For agent input, the viewer gives the overlay a cyan glow, glides it between
positions, and pulses it on clicks. With reduced motion enabled, the cursor
snaps to each position and clicks flash without expanding.
Cursor positions use the same normalized coordinates as absolute input,
independent of video quality and output scaling.

## Build from source

- Node.js 26 or newer
- pnpm 11.9.0, as declared in `package.json`
- the Rust toolchains declared in `rust-toolchain.toml` and `tools/rustfmt/rust-toolchain.toml`
- [just](https://just.systems/) and [uv](https://docs.astral.sh/uv/)
- Linux, pkg-config, and the Pixman, xkbcommon, Cairo, Pango and PipeWire 0.3
  development packages, plus libclang for the PipeWire bindings

```sh
pnpm install --frozen-lockfile
just build
```

This builds the browser viewer and embeds it in `target/release/waywire-gateway`,
alongside `target/release/waywire-compositor`. These are the current two binaries;
there is no packaged desktop distribution or installer.

Window decorations use Cairo and Pango shared libraries and the host's system
fonts. Titles request generic Sans, with system fallback for other scripts; no
font or desktop theme is bundled.

At runtime, the compositor needs FFmpeg with libx264, Xwayland, and the application
you choose to launch. It currently starts the X11 bridge even for native Wayland
applications. The compositor links the PipeWire client library. When the
`pipewire` daemon binary is installed, it starts a private daemon so the
ScreenCast portal can deliver video. Without the binary, ScreenCast reports
stream metadata only. Pass your application and arguments directly to the gateway:

```sh
./target/release/waywire-gateway \
  --compositor ./target/release/waywire-compositor \
  --public-url https://desktop.example.com \
  -- PROGRAM ARG...
```

Replace the example origin with the browser's actual origin. The gateway binds
to loopback port 8080 by default and provides no authentication; remote access
needs an authenticating HTTPS proxy. Application arguments are passed without
shell parsing. `WAYWIRE_SESSION` is a fallback for a single executable path.

The compositor draws a taskbar at the bottom of the desktop. Its window chips
focus or restore windows; clicking the focused chip minimizes it. Super+Shift+M
restores the most recently minimized window. The bar also shows the local time
and whether the agent or a browser viewer holds input. Interacting with the web
viewer takes control when remote input is enabled; there is no in-desktop
Take over button.

The gateway and compositor accept repeated `--pin 'COMMAND ARG...'` options
for taskbar application commands. Pin arguments use shell-style quoting, without
shell expansion. Clicking a pin launches its command, or focuses or minimizes
its window.

The gateway also reads pins from a TOML configuration file:

```toml
pins = ["foot", "xterm -fa Mono"]
```

By default it looks for `$XDG_CONFIG_HOME/waywire/config.toml`, or
`~/.config/waywire/config.toml` if `XDG_CONFIG_HOME` is unset or empty. A missing
default file is ignored. Use `--config <path>` to select a file explicitly; a
missing explicit file, malformed TOML, or unknown key is an error. File pins
come first, followed by any `--pin` options, and use the same quoting rules.

## Local checks

```sh
just lint
just test
just build
just fmt-check
just test-ffmpeg
```

`just test` runs the Rust and browser regression tests and ShellCheck.
`just test-ffmpeg` separately exercises the encoder contract with real FFmpeg and
requires libx264. It is excluded from the ordinary test command.

## Waymote comparison benchmark

After `just build`, run `just benchmark` in an Amp orb with Waymote installed
under `/opt/amp-wayland`. The script header lists the other development-tool
prerequisites. It downloads a pinned Anthropic demo and its Python dependency
into a temporary directory. `just benchmark --check` compares both results
against the tolerances in the script and exits nonzero for a regression or a
missing comparison.

Both runs below used 1280×720 at 30 Hz and a 30 fps, 12,000 kbps encoder
target, with xterm and Debian Chromium 156.0.8078.73 as the applications.
Waywire (0.1.0-rc.1) was built from
`f1792bb931d86d96864f94344700fd83545940d4`. Waymote gateway and streamd were
0.1.7 (protocol 2), with labwc 0.9.8. The packaged Waymote binaries did not
report a source commit. Hardware: 2 vCPUs, Intel Xeon @ 2.60 GHz, 4,025,412 kB
RAM. Date: 2026-10-31.

| Metric                             | Run 1 waywire | Run 1 Waymote | Run 2 waywire | Run 2 Waymote |
| ---------------------------------- | ------------: | ------------: | ------------: | ------------: |
| Launch to first decoded frame (ms) |           593 |           900 |           540 |           552 |
| Idle CPU (% of one core)           |           0.0 |           0.0 |           0.0 |           0.1 |
| Idle RSS (MiB)                     |         141.2 |         222.1 |         141.1 |         222.1 |
| Typing CPU (%)                     |          17.4 |          15.6 |          16.7 |          16.2 |
| Typing RSS (MiB)                   |         136.0 |         222.9 |         142.7 |         222.8 |
| Typing stream (kbps)               |         133.2 |         123.3 |         133.9 |         124.8 |
| Scrolling CPU (%)                  |          50.7 |          48.2 |          50.2 |          46.9 |
| Scrolling RSS (MiB)                |        1049.0 |        1159.2 |        1070.1 |        1162.7 |
| Scrolling stream (kbps)            |        2303.4 |        2124.1 |        2342.7 |        2142.5 |
| Input-to-screen median (ms)        |          66.1 |          67.9 |          69.0 |          67.8 |
| Input-to-screen p95 (ms)           |          76.2 |          86.8 |          77.7 |         119.3 |
| xdotool XTest                      |          pass |          pass |          pass |          pass |
| Anthropic ComputerTool + grim      |          pass |          pass |          pass |          pass |

CPU and RSS are sampled across the desktop process trees and labwc's session,
including the active application and its reparented Xwayland, but excluding the
viewer and workload driver. Earlier measurements omitted Waymote's Xwayland.
RSS is summed per process, so shared pages may be counted more than once; short-lived processes
between samples may be missed. Each workload lasts eight seconds. Bitrate is
the viewer's `/stream` WebSocket payload rate, including protocol framing, not
network-interface traffic. A lower bitrate does not establish equal visual
quality: the two implementations use different x264 presets and rate controls.
The 12 latency samples time a browser keydown to a changed pixel in the
rendered video canvas, not physical display scanout. Launch includes waiting for
the application window and navigating a prewarmed browser. Agent checks run
after browser control is released; the Anthropic screenshot implementation is
adapted to use grim, as in the acceptance scenario.

## Video encoding

The default is H.264 High with YUV 4:2:0 (`libx264` ultrafast with CABAC and
the 8x8 transform re-enabled, CRF 27), including startup,
quality presets, and automatic recovery. This works with browsers that reject
High 4:4:4 Predictive, including the tested Amp macOS embedded viewer.
The compositor still renders RGB pixels; FFmpeg converts them to YUV for encoding.
Chroma subsampling can soften small colored text.

Adaptation lowers encoded scale, then bitrate, then FPS under sustained pressure,
and recovers in reverse order without switching away from 4:2:0. These changes
restart the encoder; changing encoded scale does not resize the desktop itself.
The existing bitrate cap and low-latency settings are unchanged; short measurements
can exceed the cap while the encoder's two-second VBV buffer fills.

## Development environment

The scripts in `.agents/` are for our Amp orb development environment, not
end-user installation. `.agents/setup` prepares toolchains, dependencies, browser
assets, and debug binaries. It also builds a private, CPU-only Xwayland with
direct libei socket support for native input emulation, without replacing system
libraries.

Start the development desktop portal with:

```sh
amp orb services ensure
```

The `waywire` service builds and runs this checkout's release binaries with Foot
as a sample application, not a product requirement. It has a private Wayland
socket, separate from Amp Desktop and its bundled Labwc or Waymote binaries.
Amp supplies the authenticated portal URL and supervises the whole session.
Readiness requires `/healthz` to confirm video encoding is working.

After changing source, run `amp orb service restart waywire` to rebuild and restart
the session. This closes applications in that session. Inspect startup failures
with `amp orb service logs waywire`.

For quality-decision tracing, launch the gateway with
`RUST_LOG=info,waywire_gateway::session::quality=debug`. Each sample includes pressure
reasons, recovery vetoes, encoder counts after warmup, streaks, and cooldown state.
The browser's debug console logs decoder configuration and reset causes separately.

## Known limits

This is a prototype, not a general-purpose desktop environment. There is one
output and no workspaces, audio, or GPU acceleration. Custom surface
cursors fall back to the default shape. Browser playback requires WebCodecs H.264
High decoding; support depends on the browser and its available decoders.
The XTest bridge covers input, not desktop capture: X11 screenshot tools do not
capture native Wayland windows. A compositor screenshot API is not implemented.

## License

waywire is licensed under the MIT license. See the [`LICENSE`](LICENSE) file for more information.
