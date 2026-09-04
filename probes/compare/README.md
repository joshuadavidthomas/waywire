# Compare Waymote and VNC

This is a short, opt-in recorder, not a release or a replacement desktop.

The [first user recordings](FIRST-PAIR.md) favored VNC's update frequency with
mismatched sizes. The [second pair](SECOND-PAIR.md), with Waymote at 60 Hz and
similar sizes, shows better typical Waymote spacing but worse occasional pauses.
These are normal multi-tab sessions on the same laptop/browser, as confirmed by
Josh, rather than isolated transport benchmarks.

- [Waymote recorder](https://sprite-desktop-waymote-6ra.sprites.app/?record): LXQt 2.3, labwc 0.9.3, Waymote 0.1.2, canvas-sized output, **60 Hz target**, 8 Mbps H.264 target, audio off. This replaces the first pair's fixed 1280×720 / 30 Hz settings.
- [VNC recorder](https://sprite-desktop-v1-conflicts-6ra.sprites.app/?record): disposable XFCE 4.20 / TigerVNC 1.15 desktop, noVNC 1.7.0. The bridge is a comparison build using the existing dev.13 package metadata, not a new release.

Both URLs retain Fly's private/admin policy. The original `josh-desktop` and
`sprite-desktop-v1` are untouched. The VNC comparison service uses a separate
binary outside the installed release directory. Its X server remains running.

## Local-cursor experiment

After the second pair, Josh reported that the trailing video cursor made Waymote
feel slower, and pointer locking interrupted normal desktop use. The current
experiment keeps the browser's local arrow and leaves the pointer unlocked.
No new performance comparison is needed before judging that change by eye.

LXQt's `startlxqtwayland` sets `WLR_NO_HARDWARE_CURSORS=1` on VMs. That forces the
cursor into desktop pixels, even when Waymote requests capture without an overlay.
`../waymote/run.sh` now starts this known labwc/LXQt session directly with the flag
set to `0`. The SDK patch removes automatic pointer locking on double-click;
the explicit Lock pointer button and API remain available, but are unnecessary
for ordinary desktop use.

The live compositor environment confirms the flag is `0`. Native and streamed
screenshots show the desktop without an embedded pointer. A browser-side
`dblclick` dispatch made zero pointer-lock requests, with CSS cursor `default`
and `document.pointerLockElement === null`. This dispatch checked the browser
handler only; it did not operate the remote desktop. Responsive sizing still
reached 1552×816. ShellCheck, the Go gateway tests and all 12 SDK tests passed.
Josh subsequently confirmed that this removed the main usability difference.

Josh confirmed that the local arrow made Waymote feel close to VNC. The next
[step adds matching cursor shapes](CURSORS.md), now deployed and checked with
real text, link and window-edge cursors. This requires the patched stream daemon
as well as the gateway, plus `breeze-cursor-theme`. The 60 FPS target, bitrate,
video renderer and buffering settings remain unchanged.

## Capture a pair

1. Use the same laptop, browser, network, zoom and window size. Keep only the
   viewer being measured visible. Close other viewers of that desktop.
2. Open each URL and let it connect. Both desktops now resize to their viewer's
   canvas, including during recording. Take control in Waymote to trigger resizing,
   then let it settle before recording. Waymote uses CSS pixels like noVNC, rounds
   dimensions to 16-pixel blocks, and ignores changes within 32 pixels to prevent
   resize loops. The different control bars also affect available canvas height;
   the files record actual framebuffer and CSS sizes. The old fixed-size override
   in the VNC recorder has been removed.
3. For a first check, open a file manager in each desktop. Choose **continuous
   motion**, click **Record 30 seconds**, then drag a similarly sized window
   around continuously. Leave Waymote's pointer unlocked for the local-cursor
   experiment. Record any deliberate use of Lock pointer in your notes; Escape
   releases it.
4. Click **Download JSON** after recording stops. Repeat on the other desktop.
   A second pair in reverse order helps expose changes in network conditions.
5. Keep notes about text clarity and feel with the files. These are different
   desktop environments and file managers, so this compares the two setups,
   not transport implementations in isolation.

Stopping early is supported. Hiding the tab stops recording and adds a warning.
Changing dimensions or reconnecting also adds a warning. Do not treat a still
screen's lack of updates as stutter. For a stronger comparison, use the same
animated application in both desktops; that workload is not installed here yet.

## What the files mean

The `summary` contains update spacing p50/p95/p99, gaps over 100 ms, update counts,
browser animation timing, long-task counts/time, and measured WebSocket payload
bandwidth. Raw timestamp arrays and one-second cumulative samples remain in the
file, so a mean does not hide pauses.

- `canvasUpdateFramesMs` batches visible-canvas draws into browser animation
  callbacks. This avoids counting every VNC partial update as a separate frame.
  It measures opportunities to present an update, not physical screen refreshes.
- `canvasSubmitsMs` retains individual `drawImage` completion times for inspection.
- `animationFramesMs` measures the browser's own scheduling, not remote video FPS.
- `sdkStats` preserves Waymote's emitted stats with browser timestamps: RTT,
  latency target, lateness, decoder queue, dropped-frame counter, and the other
  upstream fields. These are Waymote diagnostics, not invented VNC equivalents.
  An idle interval with no stats events can have an empty array.
- Bandwidth counts application WebSocket payload bytes in both directions across
  every socket. It excludes WebSocket framing, TLS and IP overhead. Waymote's
  `bitrateKbps` is an encoder target; it is not the bandwidth measurement.
- Input-to-screen delay is **not measured**. The next frame after an input event
  does not prove that the input caused it. A separate visible-response test is
  needed before reporting that number.

Recording adds timestamp-array writes, socket byte counting, a one-second UI
update and one animation callback per browser frame. It does not read canvas
pixels. Long tasks omit work in decoders, workers and the GPU. No input contents,
clipboard, screenshots, socket URLs, cookies or credentials enter the files.

## Optional Sprite CPU capture

`cpu.py` samples whole-Sprite CPU and available memory once per second. It includes
unrelated processes and the sampler itself, not just desktop processes. Run it
alongside a browser recording when needed:

```sh
sprite exec -s sprite-desktop-waymote \
  --file probes/compare/cpu.py:/tmp/desktop-comparison-cpu.py -- \
  python3 /tmp/desktop-comparison-cpu.py --seconds 90 --output /tmp/waymote-cpu.json
sprite file pull -s sprite-desktop-waymote /tmp/waymote-cpu.json /tmp/waymote-cpu.json
```

Repeat with `sprite-desktop-v1-conflicts` and a different output name. The sampler
refuses to overwrite evidence. Check browser/Sprite clock offset before trimming
samples to the browser interval. No matched CPU comparison has been captured yet.

## Build and restore

For Waymote, reconstruct the source from immutable commit `90564cfb02030c494939c6fdf29cae9c4d689c67`. The fetched commit archive has SHA-256 `8cd53b5222b57e0f04448a231963ae98fbe5dbaf3fc2611340b6e89f04eb19d1`. Run this from the repository root:

```sh
repo=$PWD
commit=90564cfb02030c494939c6fdf29cae9c4d689c67
archive=/tmp/waymote-$commit.tar.gz
source=/tmp/waymote-$commit
curl -fL --retry 3 -o "$archive" \
  "https://github.com/rockorager/waymote/archive/$commit.tar.gz"
printf '%s  %s\n' \
  8cd53b5222b57e0f04448a231963ae98fbe5dbaf3fc2611340b6e89f04eb19d1 \
  "$archive" | sha256sum --check --strict
rm -rf "$source"
mkdir -p "$source"
tar -xzf "$archive" -C "$source" --strip-components=1
(
  cd "$source"
  patch --dry-run -p1 < "$repo/probes/compare/waymote.patch"
  patch -p1 < "$repo/probes/compare/waymote.patch"
)
```

The source patch adds responsive sizing, explicit-only pointer lock and separate cursor shapes. Copy the recorder and metrics as a separate probe step; those files are repository-owned comparison tools, not archive contents or patch additions:

```sh
cp "$repo/probes/compare/recorder.mjs" \
  "$repo/probes/compare/metrics.mjs" \
  "$source/gateway/examples/web/"
cd "$source"
```

Build both binaries with Zig 0.16.0, Go, pkg-config, libwayland-client and libxkbcommon development files available:

```sh
zig build test -Doptimize=ReleaseSafe
node --test gateway/sdk/waymote.test.mjs gateway/sdk/cursor.test.mjs
zig build install -Doptimize=ReleaseSafe -Dversion=0.1.2-cursor.2
```

Upload `zig-out/bin/waymote-gateway` as
`/home/sprite/waymote/waymote-gateway-comparison` and `zig-out/bin/waymote-streamd`
as `/home/sprite/waymote/waymote-streamd-comparison`. Install
`breeze-cursor-theme` and upload the current `../waymote/run.sh`.
`waymote-service.json` selects the pair through `WAYMOTE_GATEWAY` and
`WAYMOTE_STREAMD`; the original release binaries remain unchanged.
The normal demo URL still works without recording.

For VNC, run `pnpm --filter @sprite-desktop/viewer build`, then build `./bridge`
with `CGO_ENABLED=0 GOOS=linux GOARCH=amd64` and:

```text
-ldflags '-X main.buildVersion=v1.0.0-dev.13 -X main.buildSource=comparison-probe'
```

The version must match this disposable Sprite's installed metadata. Upload as
`/home/sprite/desktop-comparison-bridge`, then use `vnc-service.json`.

Sprite does not update a running service definition with PUT. To switch either
service, DELETE its exact existing service name, then PUT the chosen definition.
This briefly disconnects the viewer. For restoration, use
`../waymote/service.json` for `waymote`, and `vnc-service-original.json` for
`sprite-desktop-bridge` on **the conflicts Sprite only**. Do not run the old
installer against the modified comparison service before restoring its definition.

To undo only the local-cursor experiment on the Waymote Sprite, restore
`/home/sprite/waymote/run.before-local-cursor.sh` as `run.sh` and
`waymote-gateway-before-local-cursor` as `waymote-gateway-comparison`, then restart
only `waymote`. These backups preserve the earlier 60 FPS/responsive comparison.
To keep the local arrow but undo matching shapes, instead restore
`run.before-cursor-shapes.sh` and `waymote-gateway-before-cursor-shapes` under those
same active names. Both older launchers select the original stream daemon;
remove `WAYMOTE_STREAMD` from the service definition when restoring them.

## Verification, not comparison results

- `node --test probes/compare/metrics.test.mjs` checks percentiles, payload rates,
  partial-update batching summaries and empty data.
- `pnpm exec tsx probes/compare/smoke.ts` checks the live recording/download data
  on both Sprites through a token-isolating local proxy. It changes browser size
  from 1280×900 to 1600×1000 and verifies both remote framebuffers follow their
  canvases, remain inside their containers, and report the recording size-change
  warning. Screenshots confirmed the complete desktop, including its panels.
  The final check observed Waymote 1552×816 and VNC 1566×853; both canvases had
  1566 CSS pixels of width. These are resize checks, not motion benchmarks.
- `results/*-smoke.json` are wiring checks with different idle/resize workloads.
  They are **not evidence that one setup is faster or smoother**.
- The viewer's checks and seven tests pass; the upstream Go gateway tests pass.
  Ruff and a two-second run check the CPU sampler. Both user pairs are preserved
  and analyzed in `FIRST-PAIR.md` and `SECOND-PAIR.md`. The second pair has similar,
  stable framebuffer sizes. Controlled input-latency and matched CPU measurements
  remain uncollected.
