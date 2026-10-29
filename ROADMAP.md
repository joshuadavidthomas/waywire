# Roadmap

This roadmap comes from a review of the whole repository at `8ea8f89`. It
follows the goal in the README: a real, headless desktop that agents drive and a
person watches or takes over from a browser. The phases are in order. Work in a
later phase can start early when it doesn't depend on an earlier one.

Severity labels come from the review: **high** breaks common use, **medium**
breaks specific apps or recovery paths, **low** is a rough edge. Items marked
_(hypothesis)_ were traced in code but not reproduced against a real client.

## 1. Session stability

An agent session that dies mid-task is worse than one with a visual glitch.
Each fix here is small and independent.

- **Medium:** a silently dead input owner keeps control indefinitely. The
  control socket has no read deadline, and the server sends no pings
  (`crates/gateway/src/http/control.rs`). The client pings every second, so end
  the socket after 5–10 s of silence.
- **Medium:** RTP loss can freeze every viewer on a stale picture. Assembler
  drops are only counted and never request a keyframe
  (`crates/gateway/src/video.rs`). On a quiet desktop, `/healthz` stays `ok`.
  Send `KeyframeReadiness::Missing` when the assembler starts recovering.
- **Medium:** a 2 s stall in the command pipe ends the session. Browser command
  errors go through `fail()` (`crates/gateway/src/daemon/commands.rs`). Close
  only the offending socket, and make the video worker's readiness send
  non-blocking. _(The browser trigger is a hypothesis.)_
- **Medium:** a full compositor event queue ends the session. Clipboard events
  of up to 1 MiB are never replaced in the 4 MiB queue
  (`crates/compositor/src/event_writer.rs`). Keep only the latest clipboard
  event.
- **Medium:** a client can crash the compositor with a decoration wider than
  Cairo's 32767 px limit (`crates/compositor/src/decorations.rs`). Clamp the
  width and skip a decoration that fails to paint.
- **Medium:** TypeScript allows 6000×6000 displays, while Rust caps the total at
  3840×2160 (`crates/gateway/web/src/sdk/resize.ts`,
  `crates/protocol/src/pipe.rs`). A rejected resize closes the input socket.
  Share the pixel cap.
- **Low:** a video worker panic freezes video without an error. FFmpeg crash
  loops never become fatal. Xwayland starts with SIGTERM blocked. A slow socket
  drain can cut short process-group cleanup on shutdown.
- **Low:** a malformed browser record closes the whole input socket instead of
  being dropped. Video reconnects retry every 250 ms without backoff.

## 2. Agent control plane

This is the reason the project owns the compositor, and none of it exists yet.

- **Input arbitration.** EIS input goes straight to the seat as a separate
  source and bypasses the browser's control lease
  (`crates/compositor/src/eis.rs`). Agents and humans need one ownership model
  with explicit takeover and handback, visible in the viewer.
- **Agent queries as pipe record kinds:** window list with stable IDs, geometry,
  focus and state; a settled-frame signal; screenshots on demand at a requested
  size; per-window capture.
- **Window actions shared with the desktop.** Focus, move, resize, minimize,
  maximize and close go through the same operations as titlebar clicks and the
  window list.
- **A screenshot backend** so X11-based drivers see native Wayland windows.
- **Agent-facing endpoint.** The gateway serves a local Unix socket with JSON
  requests. Drivers and harnesses run inside the box and own their network
  connection and its authentication; filesystem permissions protect the socket.
  EIS stays available for XTest and libei drivers under the same ownership
  model. An HTTP listener can be added later on the same handlers.

## 3. Window-management policy

The desktop has overlapping windows, so these policies decide whether
mainstream apps work.

- **High:** X11 pop-ups take keyboard focus when they first draw. The commit
  handler focuses override-redirect windows such as tooltips, menus and
  dropdowns (`crates/compositor/src/shell.rs`). Chromium and Electron dropdowns
  close as they open. Skip override-redirect windows here and in the
  dead-focus fallback in `render`.
- **High:** GTK3 X11 windows with client-drawn shadows shrink on each commit
  and are drawn offset. `constrain_window` configures X11 windows with
  `geometry()`, which excludes `_GTK_FRAME_EXTENTS`. _(Traced, not
  reproduced.)_
- **High:** dialogs ignore their parent. Nothing reads `xdg_toplevel` parent or
  X11 transient-for, so clicking the main window hides its own modal dialog.
  Keep children above their parent, redirect focus to a modal child, and
  centre dialogs on their parent.
- **High:** clicking the background clears keyboard focus
  (`crates/compositor/src/lib.rs`, `pointer_button`). Only change focus when a
  window is under the pointer.
- **High:** pointer focus is recomputed only on pointer motion. A click on a
  dialog that opened under a still pointer goes to the window underneath.
  Re-run motion at the current location after map, unmap, raise, maximize and
  grab end.
- **Medium:** maximized windows can be dragged and resized. Unmaximize on
  titlebar drag and refuse resize while maximized or fullscreen.
- **Medium:** configures can be sent before a client's initial commit (from
  decoration negotiation, `focus_window`, `output_resized` and
  `set_expanded`). Guard each with `is_initial_configure_sent()`.
- **Medium:** X11 requested positions and pre-map fullscreen or maximize are
  ignored.
- **Medium:** a re-mapped toplevel loses its server-side decoration mode and
  bounds. Redundant maximize requests get no configure.
- **Low:** grabs survive window destruction. Decorations follow pending state.
  X11 size hints are ignored during resize. New toplevels get bounds without the
  titlebar. Popups aren't re-constrained after output resize. X11 `WindowState`
  entries leak on unmap. X11 minimize does nothing.

## 4. Desktop chrome and shell

The goal is a minimal desktop like the hosted sandbox computers: overlapping
windows, a way to find them, and decorations that work.

**Look:** flat and dense, in the style of Zed's dark theme. Slim 28 px
titlebars, small sans text, monochrome line icons, 1px borders, small corner
radius, and one accent colour. Avoid gradients, soft shadows and textured
backgrounds: they cost CPU to draw and smear in 4:2:0 video.

**Windows:**

- Titlebar with the title on the left and minimize, maximize and close on the
  right. Close turns red on hover. Dialogs show close only.
- Double-click the titlebar to maximize or restore. A maximized window fills the
  area above the taskbar with no border, and maximize becomes restore.
- A window menu on right-click: Minimize, Maximize or Restore, Always on Top,
  Move, Resize, Close. Move and Resize are disabled while maximized; dragging a
  maximized titlebar restores the window first.
- Edge and corner resizing with resize cursors, replacing the single 24 px grip.
- Decorate only normal and dialog windows, not splash, utility or tooltip types.

**Taskbar:** a 32 px bar along the bottom.

- Pinned apps from the session configuration, such as a terminal, browser and
  file manager. A pinned app shows as a chip even when it isn't running;
  clicking it starts or focuses the app. There is no start menu.
- Chips for the other open windows. Clicking a chip focuses or restores its
  window; the focused window's chip is highlighted.
- On the right, the control state ("Agent driving" or the human owner) with a
  Take over button, and a clock. The state is drawn by the compositor so agents
  see it in their screenshots too.
- Window placement, maximize and size bounds exclude the taskbar's area.

**Implementation:**

- One `WindowAction` type (focus, move, resize, minimize, maximize, restore,
  close, show menu). Titlebar buttons, the window menu, taskbar chips,
  keybindings and the agent API all dispatch it.
- Chrome is built from window state and the theme as a short list of nodes,
  each with a rectangle, a style and an optional action. Painting, hit testing
  and cursor shapes all read that list, replacing the separate `Layout` and
  hand-written cache comparison in `decorations.rs`.
- Cache rasterized nodes and redraw a node only when its inputs change.
- Put colours, sizes and spacing in one `Theme`, with dark and light variants.
  Pass the matching colour scheme to apps.
- Expose the node list to the agent query API, so agents can find windows and
  controls without searching pixels.
- Keep Cairo and Pango on the CPU. GPU UI toolkits such as GPUI or Rune are a
  design reference, not a dependency.

## 5. Input fidelity

- **High:** macOS keys pressed while Cmd is held stay down on the remote,
  because only `keyup` releases them (`crates/gateway/web/src/sdk/input.ts`).
  Release other held keys when Meta is released.
- **High:** acquiring control writes the remote clipboard over the user's local
  clipboard (`crates/gateway/web/src/sdk/control.ts`). Don't auto-write the
  snapshot sent with control acquisition.
- **High:** wheel input is sent as continuous touchpad scroll, without
  `axis_value120` or `axis_stop` (`crates/compositor/src/lib.rs`). Wheel zoom
  may pan and Xwayland may over-scroll. _(Client effects are hypotheses.)_
- **Medium:** Cmd+V on macOS sends Super+V. IME, emoji and mobile-keyboard text
  is dropped in apps without text-input-v3, including all X11 apps. Dead keys
  may produce double input. _(hypothesis)_
- **Medium:** relative motion and pointer constraints never reach apps. The
  constraints handler exists but its global is never created.
- **Low:** clipboard shortcuts press held modifiers twice. Japanese and Korean
  keys are missing from the key map. The EI keycode subtraction can underflow.

## 6. Sharpness and scale

- **Medium:** `preferred_buffer_scale` is never sent, so clients without
  fractional-scale-v1 render at 2× at a 1.5× output and get downsampled.
- **Medium:** Xwayland never gets a client scale or XSETTINGS, so every X11 app
  is drawn at 1× and stretched.
- **Medium:** custom cursor surfaces, used by all X11 and SDL apps, fall back to
  a plain arrow.
- **Low:** the output keeps advertising the old refresh rate after an FPS
  change.

## 7. Parity and missing protocols

- Drag-and-drop: every drag is cancelled today.
- Primary selection for middle-click paste.
- Audio, which the Amp orb Desktop already streams.
- `STRING`/`TEXT` clipboard targets for older X11 apps, keeping the clipboard
  after the copying app exits, and logging oversized transfers.
- xdg-dialog and xdg-activation.

Layer-shell, idle-inhibit and keyboard-shortcuts-inhibit can wait.

## Tests

Add these alongside the phases above:

- Window-management policy tests on the existing compositor `test_state()`
  helper: focus, map and unmap, dialogs, maximize and scale.
- End-to-end WebSocket tests: the 403 on a bad Origin, the 503 when full, and a
  disconnect releasing ownership.
- Protocol fuzzing and round-trip tests for the decoders, plus a test that Rust
  accepts every size the TypeScript resize code can produce.
- Clipboard auto-sync and macOS key handling in the viewer.
- Input and scale conversion in the compositor.

## Decisions

- **Authentication.** The browser viewer stays behind an authenticating proxy.
  The agent API is a local Unix socket, so it needs no built-in token.
- **Agent endpoint.** A gateway-owned Unix socket with JSON (see phase 2).
- **Theme.** Chrome and apps follow the viewer's light or dark preference.
