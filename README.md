# waywire

waywire runs a private, single-user LXQt desktop on a Fly Sprite. A Rust gateway serves the embedded browser viewer and WebSocket endpoints, while a Rust stream daemon captures the Wayland desktop, handles input and clipboard control, encodes H.264, and streams it to WebCodecs in the browser. It is ported from [waymote](https://github.com/rockorager/waymote).

## Requirements

- Node.js 24 or newer
- pnpm 11.9.0, as declared in `package.json`
- the Rust toolchains declared in `rust-toolchain.toml` and `tools/rustfmt/rust-toolchain.toml`
- [just](https://just.systems/) and [uv](https://docs.astral.sh/uv/)
- a Fly Sprite with URL authentication set to `sprite` and private access set to `admins`
- `SPRITES_TOKEN` in the operator's environment

## Build, Release, Provision

Install dependencies and build a release archive:

```sh
pnpm install
just release v1.0.0 REVISION
```

Provision an existing Sprite:

```sh
just provision SPRITE_NAME v1.0.0
```

Open the Sprite's private URL after provisioning. The gateway needs no public relay, Cloudflare Worker, browser-held Sprite token, or shared application secret.

## Local Checks

```sh
just lint
just test
just build
just fmt-check
just test-streamd
```

The ignored stream daemon tests require ffmpeg with libx264.

## Known Limits

The H.264 High 4:4:4 Predictive stream targets Chrome on Linux. Other browsers and hardware decoders may reject it.

Audio is not implemented.

See `LICENSE` for upstream notices.
