# Socket baseline

The private Socket desktop is the sole desktop path. Its media contract is WLR
screencopy into `streamd`, H.264 over local RTP into the Socket gateway, then
WebSocket delivery to WebCodecs. There is no selectable ext image-copy capture
path in `streamd`.

The WebRTC sibling also captures with WLR, but it encodes VP9. Do not copy its
codec setup into Socket: the Socket browser path and gateway expect the existing
H.264 access-unit contract.

## Retained binary and embedded-viewer provenance

The named jj bookmark `socket-wlr-baseline` pins commit
`3f54ee99c5af84f8150a78066b5ef2b55f013c5f`. This is the hidden `nkspolsp`
mutation whose parent is `245f043212e7133c1fc66351556436794b979c79`. It is
the recorded base for the retained daemon source described below; it is not the
retained gateway's full source revision.

The two retained executables are preserved in
`baseline/retained-socket.tar.gz`. Its members have short archival labels, not
claims that the files were built or deployed as a pair. `baseline/SHA256SUMS`
records both member hashes and the archive hash. The source files were:

- `target/performance-round3-ext/sprite-desktop-gateway`:
  `0c771df4c0fb48d99971efc3133b52a7b3f0dd0474812bd8cd3ba72ddaa6abb4`
- `target/performance-round6-pipe1m/sprite-desktop-streamd`:
  `3341878c62241739569ec6b9b0e435afea1fba12050da5914040f5bca92bab14`

The retained gateway contains `index-8aDC6sJm.js`, `index-BB6fckO3.css`, and
`recorder-DarnKGgJ.js`. A clean build at `socket-wlr-baseline` produced
`index-PLx076IE.js` and gateway hash
`3ec2a0f2a0190f50c5fd8bf0e36e409641e8c081df4eebe3470f52124d6e33ad`.
That failed reproduction remains useful: it confirms that `3f54ee99` alone is
only the daemon base, since its viewer predates the retained gateway's viewer.
The three asset hashes from that run were:

```text
91a6896bd2a87cf19e23d9273fdaa1aa31bdc801e93342b8079ce2cdc15c5624  index-PLx076IE.js
c234c4869857ec8d1d9ae4168ec9a255903de52cdb91dba638afe12ab7e98565  index-BB6fckO3.css
9bdab9ac2f55e8130588d97949963aba43347b0da9a6d7e9c67c6652efc5d541  recorder-DarnKGgJ.js
```

The named bookmark `exploration-before-socket` pins commit
`6bd58a5fcdf42dee8b6b8316dd53cfbba465d9de`. A clean workspace at that explicit
commit was built with:

```sh
jj workspace add -r 6bd58a5fcdf4 \
  --name sprite-exploration-viewer-repro \
  /tmp/sprite-exploration-viewer-repro
cd /tmp/sprite-exploration-viewer-repro
pnpm install --frozen-lockfile --ignore-scripts
pnpm --filter @sprite-desktop/stream-viewer build
cargo build --locked --release -p sprite-desktop-gateway \
  --manifest-path /tmp/sprite-exploration-viewer-repro/Cargo.toml \
  --target-dir /tmp/sprite-exploration-gateway-target
```

This build produced the retained names and these hashes:

```text
95be45113bb69f0642ae1fd30adde903538428d9f970f04dea315e42e125d47a  index-8aDC6sJm.js
c234c4869857ec8d1d9ae4168ec9a255903de52cdb91dba638afe12ab7e98565  index-BB6fckO3.css
9bdab9ac2f55e8130588d97949963aba43347b0da9a6d7e9c67c6652efc5d541  recorder-DarnKGgJ.js
```

Byte-range comparisons against the uncompressed assets embedded in the
retained gateway matched all three files exactly. An explicit jj diff from
`3f54ee99c5af` to `6bd58a5fcdf4`, limited to `apps/stream-viewer` and
`crates/gateway`, contains no gateway change and one viewer change: render-frame
diagnostics use the frame callback's `now` timestamp instead of
`drawCompletedAt` for video lateness, last presentation, rendered-frame times,
and stats-interval elapsed time. That source change accounts for the retained
main viewer asset.

The gateway rebuilt at `6bd58a5fcdf4` has SHA-256
`34e0cf101c61d2959a132a05141d5bdcf0f72fc7f40be6d271e22ec7b9df4476`,
which does not match the retained gateway. The exact embedded viewer bytes rule
out viewer source or JavaScript output as the remaining cause. This test does
not identify the remaining Rust binary input; compiler invocation, build path,
linker, or other build-environment differences remain possible and must not be
inferred from the hash alone. Both clean runs used Node 26.5.0, pnpm 11.9.0,
rustc 1.97.0 (`2d8144b7880597b6e6d3dfd63a9a9efae3f533d3`), and Cargo
1.97.0 (`c980f4866141969fab6254a680546a277789d6f0`).

Current candidate hashes belong in the observations for the run that tested
them. See [`../socket-local/RESULTS.md`](../socket-local/RESULTS.md) and each
linked run's `binaries.sha256`. This file does not label one active-worktree
build as current because release builds change as the source changes. Repeated
builds from the same fixed source now match with RustEmbed's
deterministic-timestamps feature enabled.

Within the local RTP hop, each FFmpeg process sets SSRC to its positive WLR
frame generation. Generations increase without reuse, and gateway correlation
requires an access unit's SSRC to equal its metadata generation. This identity
lets the gateway reject late packets from replaced encoders before they alter
assembly state.

## WLR source provenance

The restored WLR files came from
`target/keyframe-trial-20260906/source/crates/streamd`. That tree records base
revision `3f54ee99c5af84f8150a78066b5ef2b55f013c5f` in `SOURCE-REVISION`; the saved
files also include the retained round-six patch, so the revision alone does not
describe their final bytes. `probes/rust-desktop/round6-wlr.patch` records that
patch.

On this checkout, the archived source rebuilt with:

```sh
cargo build --locked --release -p sprite-desktop-streamd \
  --manifest-path target/keyframe-trial-20260906/source/Cargo.toml \
  --target-dir target/keyframe-trial-20260906/build
```

The rebuilt daemon SHA-256 was
`3341878c62241739569ec6b9b0e435afea1fba12050da5914040f5bca92bab14`, matching
the retained deployed daemon. The rebuild used rustc 1.97.0
(`2d8144b7880597b6e6d3dfd63a9a9efae3f533d3`) and Cargo 1.97.0
(`c980f4866141969fab6254a680546a277789d6f0`), as pinned by the archived
`rust-toolchain.toml`. The hash confirms this local, locked rebuild; compiler,
linker, host library, or build-path changes can change binary bytes.

The active worktree keeps the later FFmpeg supervision, full-resolution,
4:4:4, and full-range color fixes in `video.rs`, so its new binary is expected
to differ from the archived hash. The WLR capture, WLR damage-driven idle/resume
behavior, cursor handling, release-all control, and input-method preedit control
come from the retained source.

## Local encoder check

Local ignored tests used FFmpeg n9.0.1 with libx264. They verified the H.264 SPS
and color contract, one-frame output without stdin EOF, a one-MiB stdin pipe,
encoder restart/config supervision, and sparse frame delivery after 250 ms and
one-second idle periods at both 30 and 60 FPS. Each sparse submission produced matching metadata
and a complete RTP frame from the same live FFmpeg child. This rules out a local
raw-pipe or metadata-correlation stall for that pattern.

The later local Socket probe starts labwc and LXQt with both Rust executables.
It covers WLR damage waits, idle periods, resize generations, a real network
break, and release-all through the browser. See
[`../socket-local/RESULTS.md`](../socket-local/RESULTS.md). Fresh-Sprite checks
of clipboard, IME preedit, cursor behavior, full historical motion fidelity,
latency, and target devices remain pending.
