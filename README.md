# waywire

waywire streams a wlroots Wayland desktop to a browser. A Rust stream daemon captures the desktop and encodes H.264; a Rust gateway relays the stream, carries input and clipboard messages, and serves the browser client; the client decodes video with WebCodecs. The project began as a port of [waymote](https://github.com/rockorager/waymote).

## Requirements

- Node.js 24 or newer
- pnpm 11.9.0, as declared in `package.json`
- the Rust toolchains declared in `rust-toolchain.toml` and `tools/rustfmt/rust-toolchain.toml`
- [just](https://just.systems/) and [uv](https://docs.astral.sh/uv/)

## Sprite integration

The Sprite integration installs the headless LXQt/labwc desktop and registers it as a Sprite service. It requires:

- a Fly Sprite with URL authentication set to `sprite`
- private access set to `admins` in the Sprites dashboard
- `SPRITES_TOKEN` in the operator's environment

Install dependencies and build a release archive:

```sh
pnpm install
just release v1.0.0 REVISION
```

Provision an existing Sprite:

```sh
just sprite-provision SPRITE_NAME v1.0.0
```

Open the Sprite's private URL after provisioning. The Sprite URL policy authenticates requests before they reach the gateway.

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

See `LICENSE` for upstream notices.
