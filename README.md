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

## Requirements

- Node.js 26 or newer
- pnpm 11.9.0, as declared in `package.json`
- the Rust toolchains declared in `rust-toolchain.toml` and `tools/rustfmt/rust-toolchain.toml`
- [just](https://just.systems/) and [uv](https://docs.astral.sh/uv/)
- Linux, pkg-config, and the Pixman and xkbcommon development packages
- FFmpeg with libx264, Foot (the default terminal), and Xwayland

The compositor currently starts the X11 bridge for every session, including
sessions that launch only native Wayland applications.

## Releases

Install development dependencies, then build the binaries and package them:

```sh
pnpm install --frozen-lockfile
just release 0.1.0-rc.1
```

`just release VERSION` builds the viewer before the Rust binaries and writes
`dist/release/waywire-<version>-linux-amd64.tar.gz` and `SHA256SUMS`. The version
must match `[workspace.package]` in `Cargo.toml`; a leading `v` is optional.
Pushing a `v*` tag runs the tests and publishes a GitHub release. Versions with a
hyphen are published as prereleases. Only Linux amd64 binaries are packaged.

## Installation

A Linux amd64 machine with the required packages can host waywire; a Fly Sprite
is one example. The setup script targets Debian/Ubuntu repositories.

From a source checkout, install the system packages:

```sh
scripts/setup-desktop
```

The script uses sudo and installs Firefox from Mozilla's apt repository because
Ubuntu's snap Firefox does not work in headless containers or microVMs.

XTest input needs Xwayland 23.2 or newer built with direct libei socket support to
reach native Wayland applications. This does not translate every X11 input API:
`xdotool mousemove` uses XWarpPointer, not XTest motion. On Debian 12, install the
build dependencies listed in `scripts/setup-xwayland.sh`, then run
`bash scripts/setup-xwayland.sh`. It builds a pinned CPU-only Xwayland in
`~/.local/share/waywire-xwayland` without replacing system libraries. The launchers
prefer this private installation; `WAYWIRE_XWAYLAND_PREFIX` overrides its location.

Download the release tarball and checksums, then verify before extracting:

```sh
version=0.1.0-rc.1
release="https://github.com/joshuadavidthomas/sprite-desktop/releases/download/v$version"
curl -fLO "$release/waywire-$version-linux-amd64.tar.gz"
curl -fLO "$release/SHA256SUMS"
sha256sum -c SHA256SUMS
sudo mkdir -p /opt/waywire
sudo tar -xzf "waywire-$version-linux-amd64.tar.gz" -C /opt/waywire
sudo ln -sfn "waywire-$version-linux-amd64" /opt/waywire/current
```

The launcher expects its files at `/opt/waywire/current`. Run it as your regular
user, passing the public origin your browser will use:

```sh
/opt/waywire/current/bin/desktop.sh https://desktop.example.com
```

This opens a Foot terminal, not a full desktop environment. To start a different
application, append its executable and arguments to the launcher command. The
gateway also accepts `-- PROGRAM ARG...`; arguments are passed directly, without
shell parsing. `WAYWIRE_SESSION` is a fallback for a single executable path.

Route that origin to the gateway on port 8080 with HTTPS and authentication.
For a Fly Sprite, its private URL can provide this: keep URL authentication set
to `sprite` and private access set to `admins`.

## Local checks

```sh
just lint
just test
just build
just fmt-check
just test-compositor
```

`just test-compositor` runs the FFmpeg tests and real Wayland/X11/EI clients
against disposable compositor instances. It requires the EI-enabled Xwayland
described above, FFmpeg with libx264, a C compiler, `wayland-scanner`, Wayland/X11
and XTest development packages, `xterm`, `xdotool`, `wmctrl`, `xclip`, and
`x11-utils`. `.agents/setup` installs these prerequisites in orbs. The native
scene test checks raw pixels before encoding; the interop test exercises the
real encoder and Xwayland input and clipboard paths.

## Video encoding

The default is full-resolution RGB H.264 (`libx264rgb`, ultrafast): it avoids
RGB-to-YUV conversion and preserves colored text, at a higher bitrate than the
YUV paths. Browser/network pressure can select YUV420; encoder-only pressure
lowers FPS while keeping RGB and resolution. Packed RGB downscaling was slower
than full-size encoding in FFmpeg 5.1, so it is not a CPU fallback. All formats
retain the existing bitrate cap and low-latency settings; short measurements can
exceed the cap while the encoder's two-second VBV buffer fills.

## Amp orbs

`.agents/setup` prepares the toolchains, dependencies, browser assets, and debug
binaries for the orb snapshot, including the private CPU-only Xwayland build.
Start the desktop portal with:

```sh
amp orb services ensure
```

The `waywire` service builds and runs this checkout's release compositor and gateway,
and a Foot terminal
with a private Wayland socket. It is separate from Amp Desktop and does not use
the orb's bundled Labwc or Waymote binaries.
Amp supplies the authenticated portal URL and supervises the whole session.
Readiness requires `/healthz` to confirm video encoding is working.

After changing source, run `amp orb service restart waywire` to rebuild and restart
the session. This closes applications in that session. Inspect startup failures
with `amp orb service logs waywire`.

## Known limits

This is a prototype, not a general-purpose desktop environment. There is one
output and no panel, dock, workspaces, audio, or GPU acceleration. Custom surface
cursors fall back to the default shape. The H.264 High 4:4:4 Predictive stream
targets Chrome on Linux; other browsers and hardware decoders may reject it.
The XTest bridge covers input, not desktop capture: X11 screenshot tools do not
capture native Wayland windows. A compositor screenshot API is not implemented.

## License

waywire is licensed under the MIT license. See the [`LICENSE`](LICENSE) file for more information.
