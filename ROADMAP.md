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
The session stability items are complete.

## 2. Agent control plane

This is the reason the project owns the compositor. Reuse the tools agents
already use before building new ones, and do it in two passes.

### Pass 1: on the box, with standard tools

Agents and their harnesses run inside the box. Make existing tools work by
supporting the standard protocols and portals they speak.

Targets, most important first (checked November 1, 2026):

1. **The ChatGPT and Claude desktop apps.** Both have computer use on macOS
   and Windows, and both ship Linux desktop apps without it. OpenAI says
   [a future release will add it](https://learn.chatgpt.com/docs/linux/linux-app);
   Anthropic lists it as
   [not yet in the Linux beta](https://code.claude.com/docs/en/desktop-linux).
   Neither says how the Linux version will see or drive apps. On macOS they use
   Screen Recording and Accessibility. The likely Linux equivalents are the
   Screenshot, ScreenCast and RemoteDesktop portals and the AT-SPI
   accessibility bus, all of which waywire serves. Run both apps against
   waywire as soon as either ships Linux computer use.
2. **cua-driver** (trycua/cua, 31,430 stars; 44,241 downloads of the v0.38.0
   Linux assets). It passes the takeover acceptance scenario.
3. **computer-use-linux** (agent-sh, 780 stars; 1,566 crates.io downloads,
   first published May 2026). It needs the upstream window backend and chord
   fixes described below, proposed in
   [agent-sh/computer-use-linux#358](https://github.com/agent-sh/computer-use-linux/issues/358)
   with PRs [#359](https://github.com/agent-sh/computer-use-linux/pull/359)
   and [#360](https://github.com/agent-sh/computer-use-linux/pull/360).

Anthropic's computer-use demo and OpenAI's pyautogui sample are compatibility
checks for X11-only tools, not product targets.

The bar is the versions distributions actually ship. A tool that has to be
built from source, or a patched Xwayland, does not count as "existing". Pick
protocols by what installed tools speak, not by what Smithay makes cheap.

Agent input on Wayland goes through the xdg-desktop-portal RemoteDesktop
interface, which current Linux drivers (trycua's cua-driver, computer-use-linux)
use. Its `ConnectToEIS` call hands a driver a connection to the EIS server
waywire already runs, so the backend is a thin D-Bus service, not a new input
stack. xdg-desktop-portal-wlr provides no RemoteDesktop, so waywire implements
that backend and the ScreenCast and Screenshot backends itself. The compositor starts its own
`dbus-daemon` on a socket in its runtime directory and publishes the portal
environment there, so portals work without an inherited session bus and never
modify a host desktop's activation environment. A private `pipewire` daemon
in the same directory carries ScreenCast video. The wlr virtual-pointer and
virtual-keyboard protocols cover drivers that synthesize input directly.

| Protocol or interface           | Installed tools that use it                                    | Status                                                            |
| ------------------------------- | -------------------------------------------------------------- | ----------------------------------------------------------------- |
| RemoteDesktop portal (EIS)      | cua-driver, computer-use-linux                                 | Done; compositor's private bus                                    |
| Screenshot portal               | computer-use-linux, Pillow and pyautogui fallbacks, apps       | Done; compositor's private bus                                    |
| ScreenCast portal (PipeWire)    | GStreamer `pipewiresrc`, OBS, browsers, RemoteDesktop drivers  | Done; private PipeWire daemon, SHM video, hidden cursor           |
| wlr-screencopy                  | grim 1.4 (Debian 13, Ubuntu 24.04, Fedora 41), cua-driver      | Done                                                              |
| ext-image-copy-capture          | grim 1.5 (Fedora 42, Arch, Alpine), portal-wlr 0.8, cua-driver | Done                                                              |
| wlr-virtual-pointer             | cua-driver                                                     | Done; upstream Smithay module                                     |
| virtual-keyboard                | wtype, cua-driver, computer-use-linux fallback                 | Done                                                              |
| ext-foreign-toplevel-list       | lswt                                                           | Done                                                              |
| ext-data-control                | wl-clipboard 2.3 (Arch), clipboard managers                    | Done; clipboard and primary, no focus needed                      |
| wlr-data-control                | wl-clipboard 2.1 and 2.2 (Debian 12 and 13, Fedora 42 and 43)  | Done; clipboard and primary, no focus needed                      |
| wlr-foreign-toplevel-management | wlrctl window actions                                          | Done; activate, close, maximize, fullscreen, minimize and restore |
| Pointer constraints + relative  | Xwayland's warp emulator, games                                | Done; locks, confines and relative motion                         |
| XTest through EIS               | xdotool, pyautogui input                                       | Done; needs an Xwayland built with libei (see below)              |
| AT-SPI accessibility bus        | Screen readers, accessibility-tree agents                      | Works through the private bus; screen coordinates wrong (below)   |

### AT-SPI

Checked November 1, 2026 on `a6ff700`, Debian 13 packages (at-spi2-core
2.46, Chromium, gtk3-widget-factory), with a libatspi client on the session's
bus:

- The compositor's private bus activates `org.a11y.Bus` from the distribution
  service file, and `GetAddress` returns an accessibility bus in the session's
  runtime directory. `IsEnabled` is true.
- GTK 3 registers without configuration: 259 accessible nodes, and its named
  buttons expose the Action interface. The action itself was not invoked.
- Chromium registers only when started after `ScreenReaderEnabled` is set, and
  then exposes only its 4-node window frame. With
  `--force-renderer-accessibility` it exposes the page: the AT-SPI `press`
  action on a page button ran its click handler. This is Chromium's own
  policy, not something the compositor controls.
- **Screen coordinates are wrong.** Wayland clients do not know where their
  window is, so they report window-relative positions as screen positions.
  Chromium reported its page button at (34, 171); OCR of a grim capture found
  the label at (184, 184). GTK reports its frame at (0, 0). An agent can
  navigate the tree and run actions, but cannot click from AT-SPI extents
  alone. It needs each window's position on screen, which no standard Wayland
  protocol gives clients. Decide how to provide it only after a target driver
  shows it needs it.

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
  metadata, never PipeWire video. waywire now serves those methods, and the
  stream is a PipeWire node. On a rerun against Debian 13, screenshot and portal
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

Pass 2 is ranked by a dogfood run on November 1, 2026 at `0548b28`. An agent drove six tasks
through five drivers and recorded every call:

- **Drivers:** cua-driver 0.38.0, computer-use-linux 0.7.35 stock and with PRs #359 and #360,
  an xdotool + grim/scrot loop in the shape of Anthropic's demo, and agent-desktop 1.0.2.
- **Tasks:** a web form, editing a file through the GTK file chooser, terminal-to-browser
  paste, file-manager drag, switching windows by title, and recovery from a surprise dialog or
  a moved window.
- **Desktops:** waywire, Waymote, Xvfb with noVNC, and Selkies (linuxserver Webtop, CPU
  encoding).

The report, call log and harness are kept outside the repository. Each item below names the
measured cost behind it.

Backed, most important first:

1. **Input delivery receipts and focus events.**
   - **Measured cost.** A dialog that opened mid-task swallowed typed input in 16 of 24 runs,
     on every desktop and every pixel driver, while every call reported success. One run
     submitted a corrupted phone number.
   - **What to build.** The compositor knows which surface received each key and button
     event, and when keyboard focus moves to a newly mapped window. Report both to the agent,
     per input and as events.
   - **Why it wins.** None of the compared desktops or drivers offers this, so it is the item
     that would make waywire clearly better than X11 desktops, not just level with them.
2. **A window query with stable ids, geometry, stacking, pid and focus.**
   - **Measured cost.** On waywire no tool gives screen-space window bounds: foreign-toplevel
     has none, and cua and agent-desktop report AT-SPI window-relative bounds as screen
     positions. Every window-scoped click therefore had to be read off a full-desktop
     screenshot. On Xvfb and Selkies, xdotool and wmctrl report real geometry.
   - After a window moved, clicks at old coordinates hit the wrong window in 11 of 24 runs.
     Recovery by identity worked every time.
   - This is the window half of the agent query API in section 4. Control identity inside
     windows stays with AT-SPI, which worked on the session bus for cua and
     computer-use-linux.
3. **A per-input settle signal:** no damage for N ms since input X.
   - **Measured cost.** 7.7% of waywire screenshots (12 of 156) were taken before the screen
     settled. waywire itself settles quickly: the file chooser settles in 288 ms at p50, against
     622–683 ms elsewhere. The early screenshots came from cua returning in 96 ms on waywire,
     against 466 ms on Xvfb.
   - **What it replaces.** Fixed sleeps of 400–800 ms; 67 sleeps in the waywire runs.
   - **Limit.** It cannot see app timers: an alert behind a 300 ms page timer settled at about
     600 ms after a quiet screen.
4. **Text input through the compositor's keymap.**
   - **Measured cost.** wtype and cua type through per-string virtual keyboards, and Chromium
     drops characters from them. A punctuation-heavy line came back wrong 10 times out of 10 on
     waywire and on Waymote, and correct 10 times out of 10 with xdotool on X11.
   - **What already works.** waywire's RemoteDesktop `NotifyKeyboardKeysym` typed the same
     line correctly 5 times out of 5. Drivers should use that path or EIS for text. The agent
     endpoint should offer only that path.

Not a goal on its own:

- **A network agent endpoint with its own authentication** is the transport for items 1–4.
  Ship them first where on-box drivers can read them. Every run here was on the box, so nothing
  measured a need for remote access yet.

Dropped:

- **Window-aware or per-window streaming.** No driver read the stream. Switching by title
  worked in every run: patched computer-use-linux and cua through foreign-toplevel, and
  xdotool through Xwayland.
- **Observations taken from the rendered scene without an encode and decode round trip.**
  Agent screenshots never pass through the encoder. They come from screencopy or the portal:
  the driver call takes 45–76 ms at p50, of which grim's raw capture is 13 ms. Most of the cost
  is PNG encoding and transfer in the driver, not the compositor.

One gap outside these items: browser input-to-pixel in the viewer is 92 ms at p50 on waywire,
against 20 ms on Selkies (x264 at 30 fps, CPU only). Until it is closed, Selkies is the
better desktop for a person watching or taking over. The next step is a per-stage timestamp
breakdown in the Waymote comparison benchmark.

## 3. Window-management policy

The desktop has overlapping windows, so these policies decide whether
mainstream apps work.

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
- Clicking a pin does nothing when its child is alive but has no matching
  window, such as a wrapper that forks.
- The gateway does not validate `--pin` quoting before starting the compositor.

## 5. Input fidelity

- **Low:** a single text commit with more distinct characters than the spare
  keyboard keycodes can hold drops the excess characters for clients without
  text-input-v3. The X11 pool is limited to keycodes at or below 255. X11
  clients that have not consumed earlier keyboard events may decode them using
  a newer mapping after a spare keycode is reused, or is returned to its
  physical key, such as Zenkaku/Hankaku under a US layout, when that key is
  pressed.
- **Low:** EI clients on libxkbcommon before 1.6 that compile the EIS keymap
  at its advertised size reject it, because the size counts a trailing NUL.
  Such clients cannot map characters to keycodes. Xwayland and cua-driver are
  not affected. The fix belongs in Smithay's libei seat; see
  [Dependency currency](#dependency-currency).

## 6. Parity and missing protocols

- Drag-and-drop works between Wayland clients, between X11 clients, and in
  both directions between them through Smithay's XWM XDND bridge. The browser
  viewer, EIS and XTest agents and the virtual pointer all drive it, and a
  Wayland source's drag icon is drawn into the picture under the pointer. A
  browser takeover or handback, a disconnected agent device, or the source
  client exiting cancels the drag without a drop. Still missing:
  xdg-toplevel-drag, which Smithay `f581531` does not implement, so a Chromium
  tab dragged on `--ozone-platform=wayland` detaches into a new window where
  it is released instead of following the pointer; touch drags, since the
  seat has no touch device; and dropping files from the viewer's own machine
  onto the remote desktop.
- Audio, which the Amp orb Desktop already streams.
- Data control is served as `ext-data-control-v1` and
  `zwlr_data_control_manager_v1` version 2, both with the primary selection.
  wl-clipboard 2.3 (Arch) uses the ext protocol; 2.1 and 2.2 (Debian 12 and 13,
  Fedora 42 and 43) know only the wlr one. Copies and pastes no longer map a
  window or move keyboard focus, and they reach X11 and the viewer through the
  same ownership as other Wayland clients. Access is under Decisions.
- xdg-activation is served, with the focus policy under Decisions.

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
  workaround around `XWayland::spawn` stays.
  Keymap sizes that count a trailing NUL break libxkbcommon before 1.6
  (Debian 12 ships 1.5). `xkb_keymap_new_from_buffer` rejects such a buffer,
  and xkbcommon-rs `Keymap::new_from_fd` passes the size through unchanged.
  This affects both directions. Upstream's virtual-keyboard handler compiles
  `wtype`'s keymap at the size `wtype` sends, which counts the NUL, so on
  Debian 12 every `wtype` keymap failed and keys were dropped. The compositor
  now leaves that NUL out of `zwp_virtual_keyboard_v1.keymap` requests before
  Smithay dispatches them (`shell.rs`, `virtual_keyboard.rs`). In the other
  direction, the libei seat still advertises the sealed `KeymapFile` size,
  NUL included, in the EIS keymap event. EI clients that compile at that size
  with libxkbcommon 1.5 fail, including reis's `type-text` example and a
  libei 1.5 client using `new_from_buffer`. Xwayland ignores the EI keymap, and
  cua-driver 0.38's EIS fallback typed correctly on Debian 12. Smithay's seat
  exposes no hook for the size, so waywire does not work around it. Both
  directions are fixed by one upstream Smithay change. When it lands, drop the
  local `Dispatch` impl in `shell.rs`.
- **Crates.** Dependabot opens weekly grouped PRs for crates, npm packages,
  GitHub Actions and prek hooks. It ignores Smithay: the latest crates.io
  release, 0.7.0 (June 2025), lacks `backend_libei`, the virtual-pointer and
  image-copy-capture modules and XWM drag-and-drop, so the git pin stays and
  each bump is a deliberate port. Bump it when upstream has something waywire
  needs, such as xdg-toplevel-drag. `nix` is pinned exactly (`=0.31.3`);
  re-check it against newer releases deliberately. `zbus` stays on 5.x: 6.0
  drops the `blocking-api` feature and reworks runtime selection, so moving the
  portal thread to it is a port, not a bump.
- **Xwayland.** `.agents/setup` builds 24.1.14. Track the 26.1 release for
  pointer-warp and libei changes.

## Tests

The window-management, WebSocket, decoder, viewer and input-scale tests from
the review are in place. Remaining gaps:

- Coverage-guided fuzzing. The pipe, RTP and H.264 depacketization decoders
  have proptest properties: CI runs 256 cases of each, and `just fuzz` runs
  more. `cargo-fuzz` sanitizers need nightly `-Z` flags, which the pinned
  stable 1.97.1 rejects. The browser JSON parser
  (`ClientMessage::parse_json`) has only example tests.
- The WebSocket tests run the gateway binary against a stub compositor that
  records the command pipe. No test drives the browser viewer through a live
  gateway and compositor.
- `composed_text_reaches_wayland_client_without_text_input` failed once in a
  full ignored run because its GTK client never started within 15 seconds. It
  did not recur in 30 later paired suite runs or in 150 standalone starts. One
  unverified suspect is GTK blocking on `org.a11y.Bus` activation on the
  private session bus.

## Decisions

- **Scope.** Headless and remote only; no local hardware backend for now.
- **Authentication.** The browser viewer stays behind an authenticating proxy.
  Pass 1 agents run inside the box and use local Wayland and EIS sockets, so
  they need no built-in token. A pass 2 network endpoint brings its own.
- **Agent interface.** Standard protocols and existing tools first (see phase
  2). A custom endpoint is added only for a measured gap.
- **Dialog modality.** Wayland children redirect focus only with the xdg-dialog
  modal hint; X11 windows continue to use `_NET_WM_STATE_MODAL`. Dialog chrome
  and taskbar grouping remain based on the parent relationship.
- **Theme.** Chrome and apps follow the viewer's light or dark preference.
- **Clipboard persistence.** When the app that owns the clipboard exits, the
  compositor keeps serving its plain text: at most 1 MiB of UTF-8, the viewer
  sync limit. It offers that text as `text/plain;charset=utf-8` and
  `text/plain` to Wayland clients and as `UTF8_STRING`, `TEXT` and `STRING` to
  X11 clients. Other types, such as HTML, images and file lists, end with their
  owner, and a clipboard without kept text becomes empty. The next copy or
  viewer write replaces the kept text. A Wayland client that clears the
  selection clears it. X11 cannot tell an explicit release from an exit, so an
  X11 release keeps the text.
- **Data control.** Every client may bind both data-control globals. The
  session belongs to one trusted user, and screencopy, virtual input and
  foreign-toplevel management are open to every client for the same reason.
  A data-control source is an ordinary Wayland owner: its clipboard syncs to
  the viewer and X11, its text is kept when it exits, and its primary
  selection follows the primary-selection rules. If the compositor ever
  serves security-context-v1, sandboxed clients should lose data control
  together with those other privileged globals.
- **Primary selection.** Wayland clients get primary-selection v1, bridged both
  ways to X11 `PRIMARY`. It stays on the desktop: it is never sent to or set
  from the viewer, because browsers have no primary-selection API and syncing
  each text selection would overwrite the viewer's clipboard. Like X11, it ends
  when its owner exits.
- **`STRING`.** X11 `STRING` is ISO 8859-1. The compositor converts it to and
  from the owner's UTF-8 text, and characters outside ISO 8859-1 become `?`.
  Conversions and viewer reads are capped at 1 MiB and three seconds, and each
  dropped transfer is logged as a warning. Transfers between apps that share a
  type pass through unchanged and uncapped.
- **Activation.** An xdg-activation token raises and focuses its window when
  the compositor issued it for a pin launch, or when the client that holds
  keyboard focus created it with a serial from this seat's input no older than
  that focus. Browser, EIS, XTest, virtual-pointer and virtual-keyboard input
  all produce seat serials, so agent input grants tokens the same way human
  input does. Tokens last 30 seconds and are used once. Any other request,
  such as a token without a serial or one from a background client, only marks
  the window urgent: its taskbar chip turns amber until it gains focus. Unknown
  tokens do nothing. A browser takeover or handback voids every outstanding
  token, so activations requested under the previous driver cannot move focus
  afterwards; the same lease rule already drops foreign-toplevel requests
  during a takeover. Granted activations and foreign-toplevel activation share
  one path, which restores a minimized window or raises and focuses it.
  Launched apps never inherit `XDG_ACTIVATION_TOKEN` or `DESKTOP_STARTUP_ID`
  from the compositor's environment.
