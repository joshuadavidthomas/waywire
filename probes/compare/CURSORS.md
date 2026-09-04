# Local cursor shapes

Josh confirmed that removing the trailing video cursor made Waymote feel close to
VNC. This follow-up sends the remote cursor's image, hotspot and visibility while
letting the browser place it. Cursor positions never travel back to the browser.
The video renderer, 60 FPS target, 8 Mbps target and latency setting remain unchanged.
Only `sprite-desktop-waymote` changed.

## Implementation

`waymote.patch` applies to upstream v0.1.2 and now includes both native and gateway
changes. The deployed pair reports `0.1.2-cursor.2`; this is a local experiment,
not a published release.

- `CursorCapture.zig` uses `ext-image-copy-capture-v1` to capture the headless
  output's separate cursor. It shares the existing Wayland event loop, owns one
  pending frame, bounds images to 256×256, and commits a hotspot with its image.
  It ignores position events and suppresses duplicate image publications.
- `main.zig` waits for the virtual pointer's seat capabilities before starting
  capture. The first cold-start test caught this ordering requirement.
- `gateway/cursor.go` converts premultiplied BGRA into PNG using Go's image
  library. It caches combined shape/visibility state and sends the latest state
  over the existing controller WebSocket. A new controller gets the cursor even
  while the desktop is idle.
- The SDK uses a CSS PNG cursor, scales its image and hotspot to the displayed
  desktop, and caps it at 128 CSS pixels. It waits for the first video frame
  before calculating scale. Remote hiding sets `cursor: none`; disconnect,
  release and disposal restore the element's prior cursor. Image decoding has
  stale-result guards.

No new route, socket, worker or public SDK method was added. Explicit pointer lock
still uses the existing relative-input/video-overlay behavior. Ordinary desktop
use needs no pointer lock, and double-click no longer requests it.

## Wire additions

The existing native stdout frame header remains: version `2`, event type, two
reserved zero bytes, and a little-endian u32 payload length.

- Type `4`: width u32, height u32, hotspot X i32, hotspot Y i32, then exactly
  `width × height × 4` premultiplied BGRA bytes. Integers are little-endian.
- Type `5`: one visibility byte, either `0` or `1`.

The gateway emits one combined control message:

```json
{"type":"cursor","visible":true,"width":32,"height":32,"hotspotX":16,"hotspotY":4,"image":"data:image/png;base64,..."}
```

An empty image resets the browser to its local default. Image and visibility
updates replace older queued combined states rather than accumulating frames.
The current native capture requires ARGB8888 SHM and a normal output transform;
unsupported capture conditions stop the process explicitly.

## Cursor theme finding

The minimal desktop had no cursor theme installed. Its built-in tiny cursor
looked correct when composited into a screenshot, but its separate hardware-cursor
buffer already contained scrambled pixels. Reading the compositor's own SHM
buffer confirmed that corruption preceded Waymote's capture and PNG conversion.
Changing capture-buffer stride did not fix it; that experiment was removed.

Installing `breeze-cursor-theme` and setting `XCURSOR_THEME=breeze_cursors` and
`XCURSOR_SIZE=24` produced correct separate cursor images. The captured buffers
are 32×32, including transparent padding. The underlying compositor/rendering bug
in the built-in cursor path has not been isolated further. The trial now requires
and selects Breeze explicitly rather than relying on the missing theme.

## Evidence

Native GUI actions used cua-driver. Browser checks used the token-isolating
loopback proxy; API credentials stayed in Node. The final checks observed:

- A real FeatherPad text area produced a clean I-beam, hotspot 16,15.
- Its About dialog's author link produced a clean pointing hand, hotspot 16,4.
- Its left window edge produced a clean horizontal resize cursor, hotspot 16,15.
- All three browser cursor images were 32×32, with pointer lock off. Their PNGs
  are `results/cursor-text.png`, `cursor-link.png` and `cursor-resize.png`.
- Reloading and reacquiring input restored the same link cursor without further
  pointer movement. The before/after PNG SHA256 was
  `566629e948af561c0e4e92be5fa42a6aab9aee78dd396c0236ad115256d814d2`.
- Remote resize reached 1872×928. The streamed desktop remained cursor-free.
- A cursor-inclusive compositor reference capture temporarily produced a remote
  hide event; the browser hid its cursor and restored it after pointer movement.
  This checks visibility delivery, not every application's cursor-hiding behavior.

The temporary FeatherPad window, browser session and cua-driver service were
closed afterward. No new motion benchmark or physical input-latency claim is
made. Josh's verdict on the final matching shapes is still pending.

Validation passed with Zig 0.16.0: native tests, Go gateway tests with the race
detector, all 15 SDK tests, ShellCheck, and patch application/reversal dry-runs.
Tests cover malformed/truncated/oversized cursor events, PNG channels and alpha,
combined-state caching, queue replacement, reset, cursor scaling and hiding,
late image decoding, style restoration and explicit-only pointer lock.
