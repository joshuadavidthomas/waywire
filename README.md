# waywire

waywire streams a wlroots Wayland desktop to a browser. A Rust stream daemon captures the desktop and encodes H.264; a Rust gateway relays the stream, carries input and clipboard messages, and serves the browser client; the client decodes video with WebCodecs. The project began as a port of [waymote](https://github.com/rockorager/waymote).

## Requirements

- Node.js 26 or newer
- pnpm 11.9.0, as declared in `package.json`
- the Rust toolchains declared in `rust-toolchain.toml` and `tools/rustfmt/rust-toolchain.toml`
- [just](https://just.systems/) and [uv](https://docs.astral.sh/uv/)

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

Any Ubuntu machine with the required desktop packages can host waywire; a Fly
Sprite is one example. Use an amd64 machine with Ubuntu repositories that provide
`lxqt-wayland-session` and the other packages installed by the setup script.

From a source checkout, install the system packages:

```sh
scripts/setup-desktop
```

The script uses sudo and installs Firefox from Mozilla's apt repository because
Ubuntu's snap Firefox does not work in headless containers or microVMs.

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

Route that origin to the gateway on port 8080 with HTTPS and authentication.
For a Fly Sprite, its private URL can provide this: keep URL authentication set
to `sprite` and private access set to `admins`.

## Local checks

```sh
just lint
just test
just build
just fmt-check
just test-streamd
```

The ignored stream daemon tests require ffmpeg with libx264.

## Known limits

The H.264 High 4:4:4 Predictive stream targets Chrome on Linux. Other browsers and hardware decoders may reject it. Audio is not implemented.

## License

waywire is licensed under the MIT license. See the [`LICENSE`](LICENSE) file for more information.
