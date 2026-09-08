---
type: implementation-status
repo: sprite-desktop
status: partial
updated_at: 2026-09-08
source_outline: 003-socket-next-steps.md
---

# Socket implementation status

The five slices in `003-socket-next-steps.md` are partially complete. The active tree has one local Socket candidate. It is not deployed, and this work did not touch either retained Sprite.

## Slice 1: reproducible Socket baseline

Status: mostly complete.

- The WLR daemon source and round-six patch were restored to the active implementation.
- The exact retained daemon rebuilt to SHA-256 `3341878c62241739569ec6b9b0e435afea1fba12050da5914040f5bca92bab14` from the archived source.
- The retained pair is tracked in `probes/rust-desktop/baseline/retained-socket.tar.gz` with checksums.
- The retained gateway's embedded viewer assets match the build from revision `6bd58a5f` byte for byte. A clean Rust gateway rebuild does not match the retained executable, and the missing historical build inputs remain unknown. The records make no byte-identity claim for that gateway.
- RustEmbed now uses deterministic timestamps. The regression first failed with same-content binaries `5ca7b0…` and `7c72…`; repeated builds after the fix match.

The unresolved historical gateway hash does not block building a new identified candidate, but it leaves the retained gateway's complete build provenance open.

## Slice 2: one Socket repository

Status: implemented locally; fresh installation unverified.

- `apps/web` is the sole viewer. The Rust gateway embeds it and owns the same-origin HTTP and WebSocket path.
- VNC/noVNC, the Go bridge, Cloudflare Worker/proxy wiring, shared tickets, WebRTC, and alternate capture selection were removed.
- WLR screencopy is the only capture backend. Audio was removed from the server and package shape.
- The release archive and installer contain the gateway, stream daemon, LXQt/labwc session owner, and Socket dependencies. Their normal settings are 16,000 kbps and 60 FPS.
- The release archive contract test passes locally. It checks the two-artifact manifest, installer digest and size, runtime package set, real executable versions, and session owner. The installer suite has six Python pidfd/process-group tests.

A fresh private Sprite still needs the ordinary installer, service health, authenticated HTTP/API, and recovery checks. Agree a bounded cloud-use allowance before provisioning it.

## Slice 3: monitoring and takeover

Status: locally implemented and partly verified.

- Generation is the RTP SSRC across the exact positive FFmpeg range, ending at `i32::MAX`. Parent and media tests cover both bounds.
- The stream daemon preserves required frame metadata when replaceable cursor records change. Gateway timing records preserve frame metadata for every completed socket write. Preedit and release-all controls cross the native boundary.
- Local real-process IPC and media tests cover pipe limits, encoder lifecycle, sparse frames, queue bounds, and correlation failures. The media command runs seven ignored FFmpeg tests.
- The compositor probe passed at 30 and 60 FPS: attach while idle, a 45-second quiet period with zero decode/draw work, 35 seconds hidden and return, real Docker network loss, local held-Shift release while offline, new-controller takeover, and lowercase `a` in qterminal.
- The probe owns its container and browser, applies 15-minute limits, and cleans up only those resources.

Fresh-Sprite service restart, clipboard, IME preedit cancellation, and cursor behavior remain open. Local browser/network recovery does not substitute for Sprite suspension and service recovery.

## Slice 4: readable text and presentation

Status: partial.

- The viewer canvas uses `object-fit: contain`, and automatic resize rounds to two-pixel codec bounds instead of 16-pixel bounds. A red-first run decoded a 1920×1080 request as 1920×1072; the corrected runs produce full native 1920×1080 RGBA canvases. They also cover 1280×720.
- Raw decoded frames come from `canvas.toDataURL()`, bracketed by unchanged `grim` source hashes. Chrome reports I444, sRGB transfer, BT.709 matrix and primaries, and full range.
- At 60 FPS, whole-image PSNR at 8,000 kbps was 36.811012 dB at 1280×720 and 39.698755 dB at 1920×1080. At 16,000 kbps it was 43.267963 and 47.392151 dB against the same source hashes. This one comparison set the 16,000 kbps default.
- The older 27.935506 dB result remains recorded as invalid because it measured a CSS-clipped browser screenshot.

The moving fixture updates at about 24.5 draws/s. These runs do not establish 60 FPS, full historical motion fidelity, end-to-end latency, unique visual-frame delivery, physical presentation, or a p95 freeze bound. CPU is one sample per setting, measured from cumulative `usage_usec` for whole separate cgroups; it supports no statistical improvement claim and is not comparable to the older headed-Chrome Sprite benchmark.

## Slice 5: complete candidate verification

Status: open.

The root checks passed before the latest documentation update, and local archive, Python process-owner, real Rust IPC, media, and compositor checks have recorded passes. The parent integration pass will rerun the normal root gates after these docs.

Completion still requires one bounded fresh-private-Sprite trial of:

1. ordinary release build, provision, service start, and health reporting;
2. Fly private URL authentication and authenticated API behavior;
3. clipboard, IME preedit, cursor, resize, and input through the installed runtime;
4. suspension, service failure, reconnect, and process cleanup;
5. historical still/motion fidelity plus latency, unique-frame, physical-presentation, freeze, and target-device checks.

Do not provision until Josh agrees the Sprite lifetime and spending bound. A successful local run alone does not mark this slice complete.

## Evidence

- [`probes/socket-local/RESULTS.md`](../../../../probes/socket-local/RESULTS.md) records the latest local runs and failed attempts.
- [`probes/rust-desktop/SOCKET-BASELINE.md`](../../../../probes/rust-desktop/SOCKET-BASELINE.md) records retained source, binary, and embedded-viewer provenance.
- `scripts/build-release.test.ts` records the current archive contract.
