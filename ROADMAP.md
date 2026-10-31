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

- **Medium:** a 2 s stall in the command pipe ends the session. Browser command
  errors go through `fail()` (`crates/gateway/src/daemon/commands.rs`). Close
  only the offending socket, and make the video worker's readiness send
  non-blocking. _(The browser trigger is a hypothesis.)_
- **Medium:** TypeScript allows 6000×6000 displays, while Rust caps the total at
  3840×2160 (`crates/gateway/web/src/sdk/resize.ts`,
  `crates/protocol/src/pipe.rs`). A rejected resize closes the input socket.
  Share the pixel cap.
- **Low:** a slow socket drain can cut short process-group cleanup on shutdown.

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

| Protocol or interface           | Installed tools that use it                                    | Status                                                            |
| ------------------------------- | -------------------------------------------------------------- | ----------------------------------------------------------------- |
| RemoteDesktop portal (EIS)      | cua-driver, computer-use-linux                                 | Done; compositor's private bus                                    |
| Screenshot portal               | computer-use-linux, Pillow and pyautogui fallbacks, apps       | Done; compositor's private bus                                    |
| wlr-screencopy                  | grim 1.4 (Debian 13, Ubuntu 24.04, Fedora 41), cua-driver      | Done                                                              |
| ext-image-copy-capture          | grim 1.5 (Fedora 42, Arch, Alpine), portal-wlr 0.8, cua-driver | Done                                                              |
| wlr-virtual-pointer             | cua-driver                                                     | Done; upstream Smithay module                                     |
| virtual-keyboard                | wtype, cua-driver, computer-use-linux fallback                 | Done                                                              |
| ext-foreign-toplevel-list       | lswt                                                           | Done                                                              |
| wlr-foreign-toplevel-management | wlrctl window actions                                          | Done; activate, close, maximize, fullscreen, minimize and restore |
| Pointer constraints + relative  | Xwayland's warp emulator, games                                | Done; locks, confines and relative motion                         |
| XTest through EIS               | xdotool, pyautogui input                                       | Done; needs an Xwayland built with libei (see below)              |

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
supports the newer pointer-warp protocol. A click after an absolute
`xdotool mousemove` therefore lands at the compositor's stale pointer
position. `xdotool mousemove_relative` does reach the compositor, through EIS.

ydotool (uinput needs libinput) cannot work on this desktop.

### Waywire-specific work

- **Input arbitration.** Done. Every EIS connection, virtual pointer and
  virtual keyboard shares the browser's ownership model with explicit
  takeover and handback: a takeover releases held agent keys and buttons,
  drops later agent input, and gives browser keys the seat's own keymap; the
  viewer shows agent driving versus the takeover. Agent input resumes when
  the browser releases control.
- **Agent ghost cursor.** Agents move the same seat pointer as the browser, so
  watchers see an overlay cursor that jumps between points and cannot tell
  agent motion from human motion. On macOS and Windows the agent tool draws
  this cursor in its own overlay window; here the viewer is the only display,
  so it is waywire's job. The compositor tags each cursor update with the
  `KeyboardSource` that moved it, the browser cursor event carries that tag,
  and the viewer animates the overlay for agent motion: it glides between
  points, pulses on clicks and has a distinct look. The video stays
  cursor-free.
- **Window actions shared with the desktop.** Done. Foreign-toplevel actions
  share the desktop's window actions, including minimize and restore. Super+Shift+M
  restores a minimized window.
- **Acceptance tests,** on packaged tools from Debian 13 and Fedora 42:
  computer-use-linux or cua-driver drives a Wayland application through the
  portals, and Anthropic's demo loop drives an X11 application with xdotool
  and grim. In both, a browser watches and takes over.
  The rerunnable scenarios and prerequisites are in `tools/acceptance`.
  Results: Debian 13 computer-use-linux **fail** (window discovery), Debian 13
  cua-driver **pass**, Fedora 42 X11 **pass**.
  A Debian 13 rerun of cua-driver 0.38.0 showed `AGENT_BEFORE` and
  `AGENT_AFTER` in Foot, with `AGENT_DURING` absent during browser control.
  The paused notice remained visible after the driver's transient virtual
  input devices disconnected. A Fedora X11 rerun passed after an initial run
  acquired browser control before `AGENT_BEFORE` and failed that check.
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
  metadata-only stream. On a rerun against Debian 13, screenshot and portal
  click succeeded after installing xrandr, but computer-use-linux 0.7.32
  refused `type_text`: it requires a supported focused-window discovery
  backend before sending portal keyboard input, and none recognizes a native
  waywire Foot window. A compatible focused-window backend or upstream driver
  support is needed; the takeover scenario did not complete. On Fedora 42,
  Anthropic's ComputerTool with packaged xdotool and grim passed the X11
  takeover scenario: xterm displayed `AGENT_BEFORE` and `AGENT_AFTER`, but
  not `AGENT_DURING`, and the paused notice appeared during browser control.
  Fedora's packaged `ffmpeg-free` used the Cisco OpenH264 encoder.

  computer-use-linux 0.7.32 tries the first nonempty window list in
  [registry.rs](https://docs.rs/crate/computer-use-linux/0.7.32/source/src/windowing/registry.rs#190-267),
  with a COSMIC focused-window override
  ([target.rs](https://docs.rs/crate/computer-use-linux/0.7.32/source/src/windowing/target.rs#14-32)).
  Its options are:
  - GNOME Shell's `org.gnome.Shell.Introspect.GetWindows` or its own verified
    GNOME extension's `dev.avifenesh.ComputerUseLinux.WindowControl` D-Bus
    service ([gnome.rs](https://docs.rs/crate/computer-use-linux/0.7.32/source/src/windowing/backends/gnome.rs#16-111),
    [identity.rs](https://docs.rs/crate/computer-use-linux/0.7.32/source/src/identity.rs#9-24)).
  - The COSMIC helper, located by `COMPUTER_USE_LINUX_COSMIC_HELPER`, next to
    the executable, or on PATH ([cosmic_helper.rs](https://docs.rs/crate/computer-use-linux/0.7.32/source/src/cosmic_helper.rs#38-75)).
    It binds `ext_foreign_toplevel_list_v1` only when the COSMIC-specific
    `zcosmic_toplevel_info_v1` exists and reads activation from COSMIC state
    ([helper](https://docs.rs/crate/computer-use-linux/0.7.32/source/src/bin/computer-use-linux-cosmic.rs#292-352),
    [state](https://docs.rs/crate/computer-use-linux/0.7.32/source/src/bin/computer-use-linux-cosmic.rs#847-878)).
  - KWin's `org.kde.KWin` scripting D-Bus API
    ([kwin.rs](https://docs.rs/crate/computer-use-linux/0.7.32/source/src/windowing/backends/kwin.rs#24-55));
    Hyprland's `hyprctl` and `HYPRLAND_INSTANCE_SIGNATURE` or runtime socket
    discovery ([hyprland.rs](https://docs.rs/crate/computer-use-linux/0.7.32/source/src/windowing/backends/hyprland.rs#268-315));
    niri's `NIRI_SOCKET` or matching runtime socket and optional CLI
    ([niri.rs](https://docs.rs/crate/computer-use-linux/0.7.32/source/src/windowing/backends/niri.rs#260-304),
    [socket](https://docs.rs/crate/computer-use-linux/0.7.32/source/src/windowing/backends/niri.rs#394-433));
    i3's `i3-msg` and `I3SOCK` or runtime socket
    ([i3.rs](https://docs.rs/crate/computer-use-linux/0.7.32/source/src/windowing/backends/i3.rs#166-205));
    or labwc's `labwc.sock`, verified against the Wayland peer. The labwc
    backend uses wlr foreign-toplevel only for activation, not discovery
    ([labwc.rs](https://docs.rs/crate/computer-use-linux/0.7.32/source/src/windowing/backends/labwc.rs#111-180)).
  - Native X11/EWMH with `wmctrl` and `xprop`, not a Wayland session's Xwayland
    windows ([x11.rs](https://docs.rs/crate/computer-use-linux/0.7.32/source/src/windowing/backends/x11.rs#37-115)).

  Registry selection tries these backends rather than dispatching on
  `XDG_CURRENT_DESKTOP`; AT-SPI is not a window discovery fallback. waywire
  already exposes the wlr foreign-toplevel activated state, but the driver
  does not read it for discovery. Serving a desktop-specific API would pose
  as that desktop, so the compositor should not do that. Upstream can add a
  generic `zwlr_foreign_toplevel_manager_v1` backend that lists handles and
  reads their activated state, preserving its focus verification across portal
  consent ([server.rs](https://docs.rs/crate/computer-use-linux/0.7.32/source/src/server.rs#4174-4199)).
  The October 30, 2026 Debian 13 rerun still failed at the first `type_text`
  after screenshot and click succeeded. The MCP response reported all window
  backends unavailable, including a missing COSMIC helper binary. No takeover
  screenshots were captured because the failure preceded the first capture.

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
  entries leak on unmap. X11 client minimize requests are not handled.

## 4. Desktop chrome and shell

The compositor draws titlebars, window menus, and a taskbar into the desktop.
The taskbar shows window and pinned-app chips, the control state, and a clock.
Viewer interaction takes control through the web UI; the bar has no Take over
button.

Remaining:

- Cache rasterized chrome per node rather than redrawing the entire cached
  titlebar or taskbar when its inputs change.
- The light palette is provisional and has no selector. Pass the selected
  colour scheme to applications when theme selection is supported.
- Expose the node list to an agent query API so agents can find windows and
  controls without searching pixels.
- Minimized X11 windows do not receive iconic `WM_STATE` or
  `_NET_WM_STATE_HIDDEN`.
- Clicking a pin does nothing when its child is alive but has no matching
  window, such as a wrapper that forks.
- The gateway does not validate `--pin` quoting before starting the compositor.

## 5. Input fidelity

- **Low:** a single text commit with more distinct characters than the spare
  keyboard keycodes can hold drops the excess characters for clients without
  text-input-v3. The X11 pool is limited to keycodes at or below 255. X11
  clients that have not consumed earlier keyboard events may decode them using
  a newer mapping after a spare keycode is reused.
- **Low:** clipboard shortcuts press held modifiers twice. Japanese and Korean
  keys are missing from the key map. The EI keycode subtraction can underflow.

## 6. Parity and missing protocols

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
