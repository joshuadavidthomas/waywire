# Roadmap

This roadmap comes from a review of the whole repository at `8ea8f89`. It
follows the goal in the README: a real, headless desktop that agents drive and a
person watches or takes over from a browser. The phases are in order. Work in a
later phase can start early when it doesn't depend on an earlier one.

## Scope and bar

waywire is a headless, remote-only desktop: the best lightweight desktop to run
on a server, VM or microVM, used from a browser by people and agents. It aims
for the feel and weight of XFCE. A local hardware backend (DRM/KMS, libinput,
GPU rendering) is out of scope, but the code should not rule one out.

It has to be measurably better than Waymote, its inspiration, not only a port.
Every phase keeps the comparison below passing or improving.

### Waymote comparison benchmark

Run both desktops in the same orb with the same application, resolution, frame
rate and bitrate. Waymote (labwc, `waymote-streamd` and `waymote-gateway`) is
installed in Amp orbs under `/opt/amp-wayland`. Measure:

- Idle CPU and resident memory across all desktop processes.
- CPU and bitrate while typing in a terminal and scrolling a browser page.
- Input-to-screen latency in the browser viewer.
- Time from launch to the first decoded frame.
- Whether an unmodified XTest agent (xdotool, and Anthropic's computer-use
  demo loop with `grim` for screenshots) can drive the desktop.

Record results in a table in the README with the date, commits and hardware.
The benchmark is a repeatable script and a regression check, not a one-off
measurement.

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

This is the reason the project owns the compositor. Reuse the tools agents
already use before building new ones, and do it in two passes.

### Pass 1: on the box, with standard tools

Agents and their harnesses run inside the box. Make existing tools work by
supporting the standard protocols and portals they speak.

The bar is the versions distributions actually ship. A tool that has to be
built from source, or a patched Xwayland, does not count as "existing". Pick
protocols by what installed tools speak, not by what Smithay makes cheap.

Agent input on Wayland goes through the xdg-desktop-portal RemoteDesktop
interface, which current Linux drivers (trycua's cua-driver, computer-use-linux)
use. Its `ConnectToEIS` call hands a driver a connection to the EIS server
waywire already runs, so the backend is a thin D-Bus service, not a new input
stack. xdg-desktop-portal-wlr provides no RemoteDesktop, so waywire implements
that backend and the Screenshot backend itself. The compositor starts its own
`dbus-daemon` on a socket in its runtime directory and publishes the portal
environment there, so portals work without an inherited session bus and never
modify a host desktop's activation environment. The wlr virtual-pointer and
virtual-keyboard protocols cover drivers that synthesize input directly.

| Protocol or interface           | Installed tools that use it                                    | Status                                               |
| ------------------------------- | -------------------------------------------------------------- | ---------------------------------------------------- |
| RemoteDesktop portal (EIS)      | cua-driver, computer-use-linux                                 | Done; compositor's private bus                       |
| Screenshot portal               | computer-use-linux, Pillow and pyautogui fallbacks, apps       | Done; compositor's private bus                       |
| wlr-screencopy                  | grim 1.4 (Debian 13, Ubuntu 24.04, Fedora 41), cua-driver      | Done                                                 |
| ext-image-copy-capture          | grim 1.5 (Fedora 42, Arch, Alpine), portal-wlr 0.8, cua-driver | Done                                                 |
| wlr-virtual-pointer             | cua-driver                                                     | Done; upstream Smithay module                        |
| virtual-keyboard                | wtype, cua-driver, computer-use-linux fallback                 | Done                                                 |
| ext-foreign-toplevel-list       | lswt                                                           | Done                                                 |
| wlr-foreign-toplevel-management | wlrctl window actions                                          | Done; activate and close                             |
| Pointer constraints + relative  | Xwayland's warp emulator, games                                | Done                                                 |
| XTest through EIS               | xdotool, pyautogui input                                       | Done; needs an Xwayland built with libei (see below) |

### X11-only drivers

Anthropic's computer-use demo (xdotool and scrot), OpenAI's Python sample
(pyautogui) and mss work only through X11:

- Their input reaches Wayland windows only through an Xwayland built with
  libei. Fedora 41 and 42 and Arch ship one; Debian 12 and 13, Ubuntu 22.04,
  24.04 and 25.10, and Alpine do not. Nothing in the compositor can change this.
- Their screenshots read the X11 root window, which cannot see Wayland windows
  on any distribution.

The supported workaround is to run the agent's applications as X11 clients,
for example Chrome with `--ozone-platform=x11`. Then xdotool and pyautogui
input works on every distribution, and screenshots can use grim. Browser
takeover pauses Wayland portal, virtual-pointer and virtual-keyboard agents
on every distribution. It also pauses XTest agents when Xwayland has libei
(Fedora, Arch). Without libei (Debian, Ubuntu, Alpine), XTest input to X11
windows stays inside Xwayland and cannot be blocked by the compositor: xdotool
continues working during takeover. XSendEvent can inject input into X11 windows
on any build and cannot be blocked by the compositor. Plain
`xdotool mousemove` moves the seat pointer only while the X cursor is hidden,
because Xwayland 24.1 emulates warps only then, and no released Xwayland
supports the newer pointer-warp protocol.

ydotool (uinput needs libinput) cannot work on this desktop.

### Waywire-specific work

- **Input arbitration.** Done. Every EIS connection, virtual pointer and
  virtual keyboard shares the browser's ownership model with explicit
  takeover and handback: a takeover releases held agent keys and buttons,
  drops later agent input, and gives browser keys the seat's own keymap; the
  viewer shows agent driving versus the takeover. Agent input resumes when
  the browser releases control.
- **Window actions shared with the desktop.** Foreign-toplevel activate and
  close share the titlebar's focus and close functions. The full `WindowAction`
  type, minimize with restore, and a restore keybinding wait for the taskbar.
- **Acceptance tests,** on packaged tools from Debian 13 and Fedora 42:
  computer-use-linux or cua-driver drives a Wayland application through the
  portals, and Anthropic's demo loop drives an X11 application with xdotool
  and grim. In both, a browser watches and takes over.
  On Debian 13, the published cua-driver 0.38.0 Linux binary captured Foot
  with wlr-screencopy and used virtual pointer plus `wtype` for desktop input.
  Browser takeover dropped that input, and handback restored it. With `wtype`
  and `LIBEI_SOCKET` unavailable, its keyboard fallback used RemoteDesktop
  `CreateSession`, `SelectDevices`, `Start` and `ConnectToEIS`, and typed into
  xterm. In X11 mode it captured the xterm window and typed with XTest, but
  full-desktop capture failed with X11 `GetImage` `Match`. Wayland window
  listing returned Foot with no PID or geometry. These probes are not yet a
  repeatable portal takeover acceptance test.
  On Debian 13, computer-use-linux 0.7.32 (installed from crates.io) captured
  Foot through the Screenshot portal. With the ScreenCast stub, it called
  RemoteDesktop `CreateSession` and `SelectDevices`, ScreenCast `SelectSources`,
  then RemoteDesktop `Start`, but rejected the empty streams and never called
  `ConnectToEIS`; its click did not reach Foot. Outside COSMIC it sends
  input only through the RemoteDesktop Notify methods and reads stream
  metadata, never PipeWire video. waywire now serves those methods and one
  metadata-only stream; the scenario has not been rerun yet. On Fedora 42, Anthropic's
  computer tool and packaged xdotool typed a command into xterm, and grim
  captured it. The packaged `ffmpeg-free` lacks libx264, but the OpenH264
  fallback no longer blocks streaming; the X11 takeover and handback scenario
  remains unverified. Neither complete acceptance scenario has passed.

Add a settled-frame signal or a custom endpoint only when a measured gap in
these tools calls for one. No vendor offers a settled signal; they use fixed
sleeps.

### Pass 2: remote

Design with what pass 1 teaches. Following the WPRS idea in the original spec,
the remote side should understand windows and surfaces, not just receive
pictures and inject coordinates. Candidates: a network agent endpoint with its
own authentication, window-aware or per-window streaming, and agent
observations taken from the rendered scene without an encode and decode round
trip.

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
  keybindings and the agent API all dispatch it. Deferred until the taskbar
  lands: minimize with restore through its chip, the restore keybinding, and
  the full `WindowAction` type. Foreign-toplevel activate and close already
  share the titlebar's focus and close paths.
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

## Dependency currency

Bumping dependencies is part of the work, not a risk to avoid. Prefer
upstream code to local copies of it.

- **Smithay.** Pinned at `f581531` (2026-10-29), current upstream master at the
  time of the bump, 60 commits past the previous `d33a9c9` pin.
  `crates/compositor/src/virtual_pointer.rs` now adapts upstream
  `wayland::virtual_pointer` events to the seat's input entry points instead of
  porting the protocol by hand. Virtual keyboards likewise route through
  upstream's `VirtualKeyboardHandler`; the newest virtual keyboard's keymap is
  active on the seat, the seat's own keymap is used for browser keys while a
  browser holds the input lease, and the newest virtual keyboard's keymap
  returns on handback if it is still connected. Upstream's
  new Xwayland spawn variant, `spawn_with_inherited_env`, only controls
  environment inheritance and still has no pre-exec hook, so the signal-mask
  workaround around `XWayland::spawn` stays. `KeymapFile` still seals the
  keymap as a C string, and the libei seat still passes the sealed file's size,
  trailing NUL included, to the EIS keymap event. The bump does not fix the
  length that libxkbcommon before 1.6 rejects; fix it upstream.
- **Crates.** Compatible updates landed on 2026-10-30; re-run `cargo update`
  with the drift check below. `nix` is pinned exactly (`=0.31.3`); re-check it
  against newer releases deliberately. `zbus` stays on 5.x: 6.0 drops the
  `blocking-api` feature and reworks runtime selection, so moving the portal
  thread to it is a port, not a bump.
- **Xwayland.** `.agents/setup` builds 24.1.14. Track the 26.1 release for
  pointer-warp and libei changes.
- **Check.** Add a periodic job or `just` recipe that reports how far the
  Smithay revision, the crates and the Xwayland build are behind, so drift is
  visible before it becomes a large migration.

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

- **Scope.** Headless and remote only; no local hardware backend for now.
- **Authentication.** The browser viewer stays behind an authenticating proxy.
  Pass 1 agents run inside the box and use local Wayland and EIS sockets, so
  they need no built-in token. A pass 2 network endpoint brings its own.
- **Agent interface.** Standard protocols and existing tools first (see phase
  2). A custom endpoint is added only for a measured gap.
- **Theme.** Chrome and apps follow the viewer's light or dark preference.
