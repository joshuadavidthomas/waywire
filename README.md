# waywire

waywire is an experimental remote desktop built around a headless Rust compositor.
[Smithay](https://github.com/Smithay/smithay) manages Wayland applications and a
single seat; Pixman renders on the CPU, and FFmpeg encodes H.264. A Rust gateway
relays video, input, and clipboard messages to a browser using WebCodecs.
No wlroots compositor, desktop environment, GPU, or physical display is required.
The project began as a port of [waymote](https://github.com/rockorager/waymote).

The compositor owns the windows, output, and cursor directly. Cursor images are
not baked into the video: the controlling browser uses its native cursor, while
observers and pointer-locked clients show a positioned arrow. Cursor positions
use the same normalized coordinates as absolute input, independent of video
quality and output scaling.

## Build from source

- Node.js 26 or newer
- pnpm 11.9.0, as declared in `package.json`
- the Rust toolchains declared in `rust-toolchain.toml` and `tools/rustfmt/rust-toolchain.toml`
- [just](https://just.systems/) and [uv](https://docs.astral.sh/uv/)
- Linux, pkg-config, and the Pixman, xkbcommon, Cairo, and Pango development packages

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
applications. Pass your application and arguments directly to the gateway:

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

## Video encoding

The default is H.264 High with YUV 4:2:0 (`libx264`, superfast), including startup,
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
output and no panel, dock, workspaces, audio, or GPU acceleration. Custom surface
cursors fall back to the default shape. Browser playback requires WebCodecs H.264
High decoding; support depends on the browser and its available decoders.
The XTest bridge covers input, not desktop capture: X11 screenshot tools do not
capture native Wayland windows. A compositor screenshot API is not implemented.

## License

waywire is licensed under the MIT license. See the [`LICENSE`](LICENSE) file for more information.
