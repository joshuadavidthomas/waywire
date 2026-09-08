# Sprite Desktop

Sprite Desktop is a private, single-user LXQt desktop runtime for a Fly Sprite. The current candidate uses one path: `apps/web` in the Rust gateway, connected by same-origin WebSockets to the Rust stream daemon. The daemon captures WLR screencopy frames and sends H.264 video through the gateway to WebCodecs. It also handles input, clipboard, IME preedit, cursor updates, resize, and release-all control. Audio is absent.

The VNC runtime, Worker/proxy bridge, shared browser tickets, and alternate viewer were removed. No retained Sprite was changed, and this candidate has not been deployed.

## Requirements

- Node.js 24 or newer
- pnpm 11.9.0
- the Rust toolchain in `rust-toolchain.toml`
- a Fly Sprite with URL authentication set to `sprite` and private access set to `admins`
- `SPRITES_TOKEN` in the operator's environment
- an agreed, bounded cloud-use allowance before creating or changing a Sprite

## Build and install

Install dependencies and build a release archive:

```sh
pnpm install
pnpm release --version v1.0.0 --source REVISION --base-url https://example.com/releases/
```

The archive contains two Rust executables:

- `sprite-desktop-gateway` embeds `apps/web`, checks the browser origin, and owns the HTTP and WebSocket endpoints.
- `sprite-desktop-streamd` captures the Wayland desktop, encodes H.264 video, and handles desktop control.

After agreeing the cloud allowance, provision an existing disposable Sprite:

```sh
pnpm provision --sprite SPRITE_NAME --release v1.0.0
```

Open the Sprite's private URL after provisioning. The gateway needs no public relay, Cloudflare Worker, browser-held Sprite token, or shared application secret.

The normal gateway and installer settings are 16,000 kbps and 60 FPS. These defaults come from one controlled local comparison at 8,000 and 16,000 kbps; they are candidate settings rather than broad device qualifications. The H.264 High 4:4:4 Predictive stream targets Chrome on Linux. Other browsers and hardware decoders may reject it. Add `?record` to load the opt-in performance recorder.

## Local checks

```sh
pnpm check
pnpm test
pnpm build
pnpm format:check
```

Focused checks include `test:rust:ipc`, `test:rust:media`, the six Python process-group and pidfd tests in the installer suite, and the release archive contract test. The Rust IPC and media checks use real pipes and media processes.

The compositor probe in [`probes/socket-local`](./probes/socket-local/) builds a fresh pinned Ubuntu image and exercises the paired release binaries in Chrome. See its [latest results](./probes/socket-local/RESULTS.md).

## Current status

Local Socket trials passed idle attach, quiet damage behavior, hidden-tab return, a real network break and reconnect, held-key release, resize to native 1280×720 and 1920×1080 frames, raw canvas capture, and source stability. Generation/SSRC checks cover the exact positive FFmpeg range through `i32::MAX`. Gateway timing records retain frame metadata for every completed socket write. Preedit and release-all controls cross the native boundary.

The canvas uses `object-fit: contain` and automatically requests dimensions rounded to two-pixel codec bounds. A 1920×1080 request now produces a 1920×1080 decoded frame. Release builds enable RustEmbed deterministic timestamps; the first regression comparison differed (`5ca7b0…` versus `7c72…`), while repeated builds after the fix match.

Work remains before a fresh private Sprite trial: exercise the ordinary installer, health and authenticated API paths, clipboard, IME, and cursor behavior; then measure historical motion fidelity, latency, physical presentation, freeze duration, and target devices. The local moving fixture updates at about 24.5 draws/s, so it cannot qualify 60 FPS. The present evidence also lacks unique visual frame IDs and a p95 freeze bound.

Historical plans and probe records remain in `SPEC.v0.md`, `SPEC.v1.md`, and the older probe directories. The retained Socket binary pair remains unchanged in [`probes/rust-desktop/baseline/retained-socket.tar.gz`](./probes/rust-desktop/baseline/retained-socket.tar.gz). See [`LICENSE`](./LICENSE) for upstream notices.
