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
- Frame rate and frame age over a 500 kbps viewer link, which adaptation must
  meet by measuring what the viewer receives, and recovery once the link clears.
  This check is waywire's own; Waymote does not adapt to the link.
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
sleeps. The Pass 2 dogfood run measured that gap for screenshots taken too
early, and `WaitSettled` (Pass 2 item 3) is the settled signal.

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

1. **Input delivery receipts and focus events.** Done; see
   [`docs/input-delivery.md`](docs/input-delivery.md).
   - **Measured cost.** A dialog that opened mid-task swallowed typed input in 16 of 24 runs,
     on every desktop and every pixel driver, while every call reported success. One run
     submitted a corrupted phone number.
   - **What shipped.** The compositor serves `InputDelivery1` on the session bus it gives
     its applications. Every key, button, scroll and text commit from the portal, EIS, XTest,
     virtual-keyboard and virtual-pointer gets a receipt that names its source and its result:
     a window, given by its foreign-toplevel id, app id, title and pid; compositor chrome;
     nowhere; or dropped during a takeover. It also gets the focused window's text-field
     state from text-input-v3. `FocusChanged` events carry a cause such as `new-window`,
     `new-dialog`, `activation`, `click` or `window-request`; `WindowMapped`,
     `WindowChanged` and `TextFieldChanged` report windows appearing, retitling and
     focusing a text field. Viewer input gets the same receipt over the control channel.
   - **Proof.** `tools/receipts/receipts.py` prints receipts and events.
     `just acceptance receipts-t6a` replays T6a with cua-driver 0.38.0. With the page
     `alert()`, the phone keys land in the form while it reports no focused text field.
     With a `window.open` popup, the focus event names the popup by id and the keys land in
     it. The scenario fails without receipts, with a wrong text-field state, and without
     focus events. A computer-use-linux patch, kept outside the repository, fails
     `type_text` and `press_key` when their receipts disagree with the agent's target.
   - **What remains.**
     - A browser `alert()` or another in-window dialog moves no surface focus. Receipts
       show it only as the text field turning `none`. AT-SPI focus events are the
       complement.
     - The text-field state is a hint. X11 clients and Chromium with
       `--wayland-text-input-version=1` report `unknown`. Qt 6 stays `focused` after focus
       moves to a button.
     - Records are snapshots. Chromium maps a page as `Untitled - Chromium` and can focus
       its text field before it sets the page title (6 of 47 launches in one orb).
       `WindowChanged` reports the later title; consumers must join by id.
     - Xwayland does not say which X client used XTest, so XTest receipts go to every
       subscriber.
     - The driver patch has not been offered upstream, and no other compositor implements
       the interface yet.
2. **A window query with stable ids, geometry, stacking, pid and focus.** Done; see
   [`docs/input-delivery.md`](docs/input-delivery.md#window-list).
   - **Measured cost.** On waywire no tool gave screen-space window bounds: foreign-toplevel
     has none, and cua and agent-desktop report AT-SPI window-relative bounds as screen
     positions. Every window-scoped click had to be read off a full-desktop screenshot. On
     Xvfb and Selkies, xdotool and wmctrl report real geometry. After a window moved, clicks
     at old coordinates hit the wrong window in 11 of 24 runs. Recovery by identity worked
     every time.
   - **What shipped.** `InputDelivery1` version 2 adds `GetWindows` and a `WindowsChanged`
     signal. Each window has the receipts' foreign-toplevel id, title, app id, pid and X11 or
     Wayland; its content and its compositor titlebar in output pixels, the pixels of a
     full-screen screenshot; its stacking position, topmost first; focused, minimized,
     maximized and fullscreen; and its parent and modality for dialogs. The list is read
     from the scene the compositor draws. `GetWindows` replies after every signal for
     earlier input and after any `WindowsChanged` it reflects, so a driver can call it after
     a drag instead of sleeping.
   - **Proof.** `receipts.py --windows` prints the list, and `--watch` prints each change.
     Unit tests move a window by its titlebar, restack it and click its reported centre;
     one fails when the list keeps the old position. An ignored compositor test checks an
     xterm's listed bounds against `xdotool` and grim before and after it moves itself.
     `just acceptance window-query` moves an xterm by its own request and by a portal drag
     over Chromium, then clicks each window where the list says it shows. Each click's
     receipt must name the listed id, and grim must show the window's colour there. It
     fails with stale geometry, window-relative geometry, mismatched ids and reversed
     stacking.
   - **What remains.**
     - No driver reads it yet. cua-driver and computer-use-linux still report AT-SPI
       window-relative bounds; adding the window's `geometry` to them gives screen
       positions. Offering that upstream is the next step.
     - Popups, menus and tooltips are part of their window, not entries; their bounds are
       not listed.
     - The portal's absolute coordinates are logical, so drivers divide by `scale` at
       fractional scales. Both are the same at scale 1.
     - A client that moves its window on a timer, after the input that caused it, is
       announced by `WindowsChanged`, but `GetWindows` cannot wait for it.
   - This is the window half of the agent query API in section 4. Control identity inside
     windows stays with AT-SPI, which worked on the session bus for cua and
     computer-use-linux.
3. **A per-input settle signal:** no damage for N ms since input X. Done; see
   [`docs/input-delivery.md`](docs/input-delivery.md#settling).
   - **Measured cost.** 7.7% of waywire screenshots (12 of 156) were taken before the screen
     settled. waywire itself settles quickly: the file chooser settles in 288 ms at p50, against
     622–683 ms elsewhere. The early screenshots came from cua returning in 96 ms on waywire,
     against 466 ms on Xvfb.
   - **What it replaces.** Fixed sleeps of 400–800 ms; 67 sleeps in the waywire runs.
   - **What shipped.** `InputDelivery1` version 3 adds `WaitSettled`. A driver names its input
     by its first receipt's serial, such as a key press, and the compositor replies once the
     output, or one window from `GetWindows`, has had no damage for a quiet window since that
     input: 300 ms by default, 10 ms to 10 s. A separate `timeout` result says the screen was
     still changing. Damage is the output damage tracker's per-frame damage after occlusion,
     from every client including Xwayland and from the compositor's chrome; the cursor never
     counts, because it is never in a screenshot. A commit that has not reached a frame yet
     holds the answer until that frame, and the quiet window starts no earlier than the
     caller's own latest input, so a key's release is waited out too. The reply follows every
     earlier signal, like `GetFocus`, so it is also a barrier.
   - **Proof.** `receipts.py --settle -- COMMAND` runs an input and prints the settle time. In a
     live session on the orb, an xterm keystroke settled 0–1 ms after the key's receipt (10 of
     10), a shell loop that printed every 200 ms settled after 1013 ms, and Chromium page loads
     settled after 1.4–2.9 s (Wikipedia, debian.org, GitHub), fully drawn in the screenshot
     taken after the wait. Unit tests check that a repaint after the input restarts the quiet
     window, that a timeout is reported as `timeout`, that a wait from a press also waits out
     the release, that a window wait ignores damage elsewhere, and that a receipt's serial is
     refused to other processes. An ignored compositor test times an xterm keystroke over the
     bus. `just acceptance settle` clicks a page that repaints every 150 ms for 1200 ms, waits
     on the click's receipt and takes a grim screenshot as soon as the wait returns: it must
     show the final colour and match a screenshot 1.5 s later. It also checks that an endless
     animation times out while a still xterm settles. It fails when the wait answers at once,
     ignores damage, or keeps the quiet window from the input.
   - **What remains.**
     - It cannot see app timers: an alert behind a 300 ms page timer settled about 600 ms after
       the screen went quiet. Anything that pauses longer than the quiet window before it
       redraws, such as a page waiting on the network, settles early; so did a shell command
       that slept 500 ms before printing. The spec says so; drivers still check the screenshot.
     - A blinking caret, a spinner or a video in the watched area keeps it changing. The
       default quiet window is below Chromium's and GTK's caret blink phase, and a window
       scope ignores animation elsewhere, but a busy area ends in `timeout`.
     - Changes are timed per rendered frame, so a lower viewer frame rate times them later.
     - No driver calls it yet. cua-driver and computer-use-linux would call it after each
       action, before their screenshot, in place of their sleeps; offering that upstream is the
       next step.
4. **Text input through the compositor's keymap.** Done; see
   [`docs/input-delivery.md`](docs/input-delivery.md#typing-text).
   - **Measured cost.** wtype and cua type through per-string virtual keyboards, and Chromium
     drops characters from them. A punctuation-heavy line came back wrong 10 times out of 10 on
     waywire and on Waymote, and correct 10 times out of 10 with xdotool on X11.
   - **Cause.** wtype uploads a keymap per string and numbers its distinct characters from
     evdev code 1, in order of appearance; cua-driver and patched computer-use-linux type
     through wtype. Those codes are the positions of Escape, the digit row, Backspace, Tab,
     Control and the F keys, and the compositor passed them through with wtype's keymap.
     Chromium takes a key's DOM `code`, `keyCode` and editing commands from the position. In
     `TL1 a?b(c)d!e:f"g{h}i<j>k_l+m~n`, `:` is the 14th distinct character, on Backspace, and
     erased the `e` before it in 10 of 10 runs; moving `:` to 14th place in another string
     erased whatever preceded it. An all-ASCII string also lost characters on Escape,
     ControlLeft and F1, and one on F5 reloaded the page. A Wayland protocol trace of Chromium
     showed wtype's keymap arriving before every key, no modifiers and no key repeat, so it was
     not a keymap race, a missing keysym, modifier state or repeat.
   - **What shipped.** The compositor no longer activates a virtual keyboard's keymap. It
     decodes each key with the client's keymap and modifiers, and presses the key that has the
     same keysym in the seat's keymap, with the Shift or AltGr its level needs as modifier
     state, as a real keyboard would. Characters the seat's keymap lacks go on its empty RO and
     YEN keys for Wayland clients, because Chromium drops key codes above 255, and on the spare
     keycode pool for X11 clients. wtype, cua-driver and computer-use-linux type correctly with
     no driver change, and each key the driver sends still gets one receipt.
   - **Proof.** A unit test types the line through a wtype-style keymap and decodes what the
     client receives with the keymap the client has: the keys are a US keyboard's, `:` is
     Shift+`;`, nothing lands on Backspace, and each key gets one receipt. It fails without the
     change. `typeloss/cul-patched`, no longer a known failure, and a new `typeloss/wtype` cell,
     which types the line with Debian's wtype, passed 5 of 5 suite runs on Debian 12 and 13; with
     the change reverted both submit `d!f` for `d!e:f`. In the orb, wtype typed the line
     correctly 10 of 10 times into Chromium and all of printable ASCII intact, typed `é€ü` and
     Greek into Chromium, foot and xterm, and `wtype -M ctrl a` selected all.
   - **What remains.**
     - Chromium drops characters outside the Basic Multilingual Plane, such as emoji, typed as
       keys, as it did before.
     - Only two empty character keys exist under a US layout, so text with many characters
       outside the seat's keymap changes the keymap often. A layout that uses RO and YEN, such
       as Japanese, falls back to key codes above 255, which Chromium drops.
     - A modifier a driver sets that the keysym does not consume applies on the seat's key:
       `wtype -M shift 1` types `!`, as on a real keyboard, not `1` with Shift held.
     - Other compositors still pass a virtual keyboard's codes through, so drivers that want
       the same result everywhere should type keysyms through the RemoteDesktop portal's
       `NotifyKeyboardKeysym`, which typed the line correctly 5 times out of 5 before this
       change, or EIS. The agent endpoint should offer only that path.

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

One gap outside these items is closed: viewer latency. The dogfood run measured a browser
keydown to a changed pixel in the viewer at 92 ms p50 on waywire, against 20 ms on Selkies
(x264 at 30 fps, CPU only). The Waymote comparison benchmark now times each stage of that
path. Frames that answer input skip the playout delay, frames are drawn as soon as they
decode, and Chromium shows them through a `<video>` element. Timed from outside both desktops
in the same orb (an XTest key into the viewer's browser window to that window's X damage,
40 keys), waywire with OpenH264 measured 14.0 ms p50 and 16.8 ms p95, against 15.7–16.1 ms
and 18.7–21.7 ms on Selkies. The benchmark fails if waywire's viewer latency exceeds 25 ms
p50 or 35 ms p95.

### Pass 3: drivers that use the interface (proposed)

Pass 3 is ranked by a second dogfood run on November 2, 2026, at `b57ba38`. The run replayed the
first run's passing call sequences against the same tasks, drivers and desktops, with patched
computer-use-linux built from upstream `292376c`. It added an interface-aware driver: patched
computer-use-linux behind an MCP wrapper that reads receipts, `GetWindows` and `WaitSettled`.

- **What Pass 2 fixed.** The punctuation line now types correctly 10 of 10 times on waywire with
  wtype, cua-driver and both computer-use-linux builds; on Waymote, cua still gets it right 0 of
  10 times. A key reaches foot's pixels in 16.9 ms p50 without a viewer, down from 33.7 ms;
  Waymote takes 34.1. Through the browser viewer, with the first run's method, it takes
  17.0–19.4 ms p50, down from 92; Waymote's viewer takes 95.8 and noVNC 115.5. Selkies, run 1's
  leader, takes 18.9 ms p50 and 33.5 ms p95, against waywire's 24.5 ms p95.
- **What it did not fix by itself.** Drivers that do not read the interface fail exactly as
  before:
  - A mid-task dialog swallowed typed input in 19 of 24 naive T6a runs on waywire, and in every
    naive run on Waymote (6 of 6) and Xvfb (13 of 13) and Selkies (13 of 13). Every call reported
    success.
  - Edits at a moved window's old coordinates were lost in 12 of 12 waywire T6b runs.
  - 9.8% of stock-driver screenshots on waywire were taken before the screen settled (15 of 153).
  - With the interface-aware driver, all 26 of 26 waywire runs passed. It reported the
    misdirected keys in 6 of 6 T6a runs and recovered each time. No T6b edit used stale
    coordinates. 1 of 32 screenshots was unsettled, and it slept 0 times. It raised no false
    alarm in the 23 runs of other tasks.

Backed, most important first:

1. **Bring the interface to agents through the reference wrapper.** Done.
   - **Measured cost:** every number in the second bullet above. On every desktop and every
     stock driver, the agent cannot tell that its typing went into a dialog or that its click
     hit the window underneath. Only waywire can answer that, and only for a driver that asks.
   - **What it replaces:** a screenshot after every action, sleeps of 400–1500 ms, and
     coordinates read off a full-desktop screenshot.
   - **Shipped: the reference wrapper.** `tools/receipts/mcp_wrapper.py` runs any stdio MCP
     driver as its child (`mcp_wrapper.py -- cua-driver mcp`), so it receives the driver's
     receipts with no patch. It fails a typing or key call loudly when the keys reached
     another window than the agent directed them to, reached no window, or reached a window
     whose text field lost focus without the agent's input, and a typing call none of whose
     keys reached a focused text field. It waits with `WaitSettled` from each action's first
     receipt before it returns, and adds `get_windows` and `wait_settled` tools; see
     [`docs/input-delivery.md`](docs/input-delivery.md#drivers).
     - **Proof.** `just acceptance wrapper-t6a` replays T6a's alert and popup variants
       through stock cua-driver 0.38.0 and computer-use-linux `292376c`, bare and wrapped.
       Bare, every call reports success while the phone keys go to the alert or the popup.
       Wrapped, the phone call fails and names the in-window dialog or the popup, and no
       earlier call fails. Behind the wrapper, typing after the driver's own click into
       another window passes, and a wait during an endless animation reports a timeout. It
       passed 4 of 4 runs on the orb. Unit tests replay recorded traces of those runs
       through the wrapper's decision logic; each fails with its check removed.
     - **Cost.** The wait adds the quiet window to each action: 304 ms p50 in Chromium and
       289 ms in gnome-text-editor at the default 300 ms, 154 and 138 ms with
       `--quiet-ms 150`, with no timeouts. Those runs only typed; after a click, GTK 4's
       blinking caret kept waits from settling until item 2 fixed it.
   - **Deferred: driver patches.** The interface is waywire's own; no other compositor serves
     it, so a patch to computer-use-linux or cua-driver would add a code path that does
     nothing elsewhere. Offer one only once waywire has users who would ask for it. The
     likeliest form then is a waywire window backend in computer-use-linux, which already
     keeps one backend per compositor. A cross-compositor protocol or portal extension waits
     on other compositors wanting the same signals.
   - **Limit:**
     - An in-window `alert()` is detected only as the text field turning `none` with no input
       of the driver's own to explain it.
     - X11 clients and Chromium on text-input v1 report `unknown`, so for them only
       window-level misdirection is caught.
     - A driver gets the checks only when it runs behind the wrapper.
   - **This is the item that makes waywire clearly better than the alternatives.** Xvfb,
     Selkies and Waymote have no equivalent signal, and their naive runs failed the same way.
2. **Settle without waiting out the text caret.** Done.
   - **Measured cost:** the aware driver's per-action overhead is 318 ms p50 in Chromium, which
     is the 300 ms quiet window. In GTK 4 windows (gnome-text-editor and its file chooser) it is
     776 ms p50, and 8 of 36 waits ran into the 5 s timeout. Tasks in GTK 4 windows took 2–8
     times longer than with the stock driver and fixed sleeps: T2 14.6 s against 1.9 s, T5 7.0
     against 1.0, T6b 9.7 against 1.7. A 400-frame capture shows the only damage between inputs
     is a 1×17 px strip at the caret. The dogfood run's timeouts followed clicks, a window
     activation and key presses.
   - **What it replaces:** the caller's choice between a long quiet window and a timeout in
     focused text views.
   - **Why the wrapper's measurement disagreed: the input was keys, not clicks.** GTK 4 fades
     its caret out and in over 300 ms each, with holds of 300 ms less one frame between
     (`gtk-cursor-blink-time` 1200 in quarters; 284 ms p50 in a damage trace). A key that edits
     a text view restarts the blink with the caret solid for 600 ms, so a wait after it settles
     at 300 ms. A click does not, and a 300 ms quiet window seldom fits between fades. The
     wrapper's measurement typed `ab` and pressed BackSpace only. In the acceptance image with
     patched computer-use-linux behind the wrapper, 12 actions 0.4 s apart, before the fix:

     | gnome-text-editor, before the fix | Debian 13 (GTK 4.18.6, 48.3) | Debian 12 (GTK 4.8.3, 43.2) |
     | --------------------------------- | ---------------------------- | --------------------------- |
     | Keys                              | 293 ms p50, 0 timeouts       | 265 ms, 0                   |
     | Clicks into the text              | 5006 ms, 8 timeouts          | 5007 ms, 9                  |
     | Clicks, `cursor-blink` false      | 302 ms, 0                    | 303 ms, 0                   |
     | Clicks, quiet 150 ms              | 153 ms, 0                    | 153 ms, 0                   |

     Turning the GNOME `cursor-blink` setting off turns the slow behaviour off, and the damage
     traces show why: with blinking on, 1,057 of 1,377 damaged frames on Debian 13 changed only
     the caret; with it off, 23 of 314. A pause of 2.5 s instead of 0.4 s changed nothing in the
     wrapper's run. The distribution did not matter.

   - **Real users hit it.** A session as waywire starts it has no settings portal, so GTK keeps
     its default of a blinking caret, on both Debian releases the acceptance suite covers. An
     agent clicks into text fields all the time.
   - **What shipped.** The compositor tracks each text-input-v3 object's cursor rectangle. While
     a text field is enabled, a change in its window no larger than that rectangle, widened by
     2 px on each side, is not damage for `WaitSettled`. The change is matched by size, not by
     place, because GTK 4.8 reports the caret's place only when the text changes, so after a
     click it blinks one click away from where it was reported. Damage in that window is first
     narrowed to the pixels that changed, against a copy of the previous frame kept only while
     a field is focused, because GTK 4.8 damages its whole surface for each caret frame. The
     default quiet window stays 300 ms (below). No message changed. Rejected: a `caret` flag on
     `settled`, which would still leave the caller waiting out the timeout.
   - **Proof.** A compositor unit test fails without the change: a 1×17 px caret change, a
     whole-surface commit that changes only the caret, and a caret-sized change away from the
     reported rectangle all leave the wait at 0 frames; a typed glyph beside the caret, a
     caret-sized change in another window and one after the field is disabled count.
     `just acceptance settle-gtk` clicks into gnome-text-editor and requires the wait after the
     click to settle, a 500 ms quiet window to settle with no changed frame while the caret
     blinks, and only a caret-sized area of the editor to change over 1.2 s. With the change,
     clicks take 302 ms p50 with 0 timeouts in 12 on Debian 13 (max 319) and 306.5 ms with 0 on
     Debian 12 (max 325);
     keys are unchanged.
   - **The default quiet window stays 300 ms.** In the dogfood T1 and T6a form, filled by keys
     through the wrapper, half of the runs with the surprise alert, a screenshot after each of 13 or 14 actions,
     compared with one 1.5 s later: at 300 ms no screenshot missed a change to the page in 216; the misses were
     Chromium's toolbar sign-in chip collapsing and the taskbar clock. At 150 ms, 4 of 108
     screenshots missed the page's `alert()`, which opens 300 ms after Phone takes focus: every
     alert run. A 150 ms default would turn a page timer of 150–300 ms into a missed dialog.
   - **Limit:**
     - Applications without text-input-v3 cursor rectangles, such as X11 terminals and
       Chromium on text-input v1, and spinners or fading scroll bars still keep the screen
       changing.
     - In the focused field's window, any change no larger than the caret is ignored, such as
       a 1 px step of a progress bar.

3. **Viewer key repeat follows the browser.** Done.
   - **Measured cost:** one viewer keystroke became 268 characters in foot while the viewer
     page was busy, because its key release arrived late.
   - **What it replaces:** client-side repeat of a key whose release is late.
   - **Cause.** The viewer sends a key's press from its `keydown` handler and its release from
     its `keyup` handler. When the page's main thread is busy as the key goes up, the `keyup`
     handler runs late and the release leaves the browser late; the gateway and compositor
     passed it on within 1 ms. Until then the client repeated the key on its own timer, from
     the seat's repeat info of 25 a second after 200 ms. The compositor ignored the viewer's
     `keyRepeated` records. With the page busy for 3 s at each keyup, `abcd` reached foot as
     288 characters, 72 per key: 1 + (3000 − 200) / 40.
   - **What shipped.** While a browser holds input, the seat's repeat info is rate 0, so
     Wayland clients and Xwayland do not repeat keys themselves. Each `keyRepeated` record
     types its key once more, with a receipt like any key. `wl_keyboard` before version 10 has
     no repeat event, so a repeat reaches the client as a release and a press, as X11
     autorepeat does. Modifiers, keys the compositor kept for itself and keys already released
     are not repeated. Handback, including a viewer disconnecting, releases the browser's keys
     and restores client repeat. Agent input is dropped while a browser holds input, so no
     agent key loses its repeat. No message changed. A cap on how long a key can be held
     without a fresh record was rejected: the client would still repeat for the length of the
     cap, so a late release would still type extra characters.
   - **Proof.** A compositor unit test fails without the change: a client must be told rate 0
     on takeover, receive one character per browser repeat with a receipt for each, nothing for
     a repeated modifier or a repeat after the release, and get its repeat back on handback.
     `just benchmark --check` now types `busy` through the viewer while every keyup reaches the
     page a second late, then holds `x` in the browser for 12 autorepeats over the DevTools
     protocol, and requires exactly `busy` and 13 `x` in foot and in xterm. On `6688c5e` foot
     received 22 of each letter and 7 `x`, and xterm 21 and 7; with the change both receive the
     exact text. The benchmark passed twice with the change. Viewer keydown to changed pixel
     took 10.4 and 10.2 ms p50, 13.3 and 21.0 ms p95; at `6688c5e` on the same orb it took 10.8
     and 11.0 ms p50, 20.2 and 27.6 ms p95.
   - **What remains.**
     - A held key repeats at the browser's delay and rate, not the desktop's.
     - A key held while the page is busy repeats only as the page handles the browser's
       repeats, so it pauses during the stall.
     - A late release still keeps the key logically down until it arrives, which an app that
       acts on held keys, such as a game, sees.
     - Applications see each repeat as a release and a press. Smithay's seat is version 9;
       version 10 would send them as repeats.
     - The dogfood run's probe did not stall the page enough to reproduce its ~11 s late release
       here; the reproduction busies the page at each keyup instead.
4. **Close the one-frame tail in key to pixel.**
   - **Measured cost:** without a viewer, waywire's p50 equals the X11 desktops' (16.9 ms
     against 16.5–17.2). Its p95 is 32.8 ms against 18.2–18.6, because 3 of 30 keys waited one
     extra frame.
   - **What it replaces:** an occasional missed frame deadline after a client commit.
   - **Work:** trace when the key's commit arrives relative to waywire's frame deadline and
     fix the scheduling.
   - **Limit:** worth about one frame on 10% of inputs. The viewer's p95 (24.5 ms) already beats
     Selkies' (33.5 ms).
5. **Faster portal screenshots.** Done.
   - **Measured cost:** a computer-use-linux screenshot takes 53 ms p50 on waywire against 31–33
     on Xvfb and Selkies. waywire's Screenshot implementation takes 30 ms of that; grim takes
     12.8 ms.
   - **What it replaces:** about 17 ms of portal overhead above raw capture.
   - **Limit:** about 20 ms per screenshot, under 0.1 s per task. Do it only if it is cheap.
   - **Cause.** PNG encoding. Timed per stage over 30 calls on a 1920×1080 desktop in a 2-vCPU
     orb, Cairo's `write_to_png` took 44.0 ms p50 of the backend's 45.8 ms. The frame copy took
     0.9 ms, the fsync 0.8 ms and pruning 0.1 ms; xdg-desktop-portal 1.16 added about 1.5 ms.
     Cairo uses zlib's default level and cannot be given another.
   - **What shipped.** The backend encodes the frame as 8-bit RGB with the `png` crate's fast
     deflate. The frame is opaque, so nothing is lost: the file is pixel-identical to grim's
     capture of the same frame, as Cairo's was. Files are larger: 40 KB instead of 15 KB for a
     terminal, 504 KB instead of 368 KB for a Wikipedia page.
   - **Proof.** Same orb, 30 `org.freedesktop.portal.Screenshot` calls each, timed from the call
     to reading the returned file: 44.8 → 6.5 ms p50 for a terminal, 52.4 → 8.6 ms for a
     full-screen web page. grim took 52 ms there. Encoding a live Wikipedia frame alone went from
     68.6 to 7.1 ms. computer-use-linux was not rerun on the benchmark host.

Not waywire's to fix, tracked here because they cost the runs:

- agent-desktop has no input path on waywire. Its pointer and keyboard actions require
  `org.gnome.Mutter.RemoteDesktop`, and Chromium fields expose no EditableText. It passed 4 of
  18 cells on waywire, every one of them through AT-SPI actions alone, against 12 of 12 on Xvfb
  and Selkies in the first run. The fix is an upstream portal or EIS backend in agent-desktop;
  waywire already serves both. A Mutter D-Bus compatibility shim for one driver is not worth
  its surface.
- Stock computer-use-linux 0.7.35 has no window backend or `press_key` on waywire: T5 by title
  passed 0 of 3, and the per-field T6a cell had to send keys as text. Upstream `292376c` fixes
  both; it needs a release.

Dropped, or shown not to matter by the runs:

- **A network agent endpoint.** Still no measured need: every driver ran on the box.
- **Window-aware or per-window streaming.** Still no driver reads the stream.
- **Observations from the rendered scene.** Driver screenshots take 32–54 ms p50 on waywire;
  grim's raw capture takes 11.8 ms.
- **Titlebar and taskbar controls in an agent query API** (section 4). No task needed a chrome
  control; window switching went through foreign-toplevel or `GetWindows` every time.
- **Viewer latency work.** waywire's viewer is now the fastest of the four measured:
  17.0/24.5 ms p50/p95, against Selkies' 18.9/33.5.
- **Matching Selkies' encoder or transport.** Selkies is ahead of waywire only where every X11
  desktop is: the key-to-pixel tail and screenshot speed (items 4 and 5), and stock-driver
  success, whose causes are in the drivers.

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
- Expose the node list to an agent query API so agents can find controls
  without searching pixels. Windows are listed by `GetWindows` (phase 2, Pass 2
  item 2); the titlebar's and taskbar's own controls are not.
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
  upstream's `VirtualKeyboardHandler`; the compositor reads each key's keysym
  from the client's keymap and presses the seat key that has it, so the seat's
  keymap stays active (Pass 2 item 4). Upstream's
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
  The XWM pairs an X11 window with its `wl_surface` only through
  xwayland-shell-v1, which Xwayland 23.1 added. Xwayland 22.1 (Debian 12)
  names the surface with a `WL_SURFACE_ID` client message instead, which
  `f581531` stores but never matches, and `X11Surface::set_wl_surface` is
  crate-private, so the compositor cannot pair them itself. Every X11 window
  was mapped but never drawn. `Cargo.toml` therefore patches Smithay with one
  commit on top of `f581531`, branch `xwayland-wl-surface-id` of
  `joshuadavidthomas/smithay`, that matches the message and the surface's
  creation in either order. The message can also arrive after the surface's
  first buffer commit, so `xwayland.rs` replays that commit once the surface
  is associated. Drop the `[patch]`, but not the replay, when the Smithay
  change is upstream.
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
- `just benchmark --check` sometimes fails with "waymote viewer did not decode
  a nonblank frame" (4 of about 15 runs on one orb). Only the Waymote side is
  affected. Waymote 0.1.7's `/waymote.js` resets its decoder when more than 6
  chunks are queued during the GOP replay on connect, and an idle desktop never
  sends another keyframe, so the viewer stays blank. The fix is upstream in
  Waymote; a run that hits it is rerun.

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
- **Input delivery transport.** Receipts and focus events are a D-Bus
  interface, `io.github.joshuadavidthomas.waywire.InputDelivery1`, on the
  session bus the compositor gives its applications. cua-driver and
  computer-use-linux already connect to that bus for AT-SPI and the portal,
  and they already depend on zbus, so a driver adds one match rule and two
  calls. `GetFocus` replies only after every signal for earlier input, so a
  driver can use it as a barrier instead of a sleep. Rejected alternatives:
  - A Wayland protocol extension. Drivers inject input through EIS,
    the portal or XTest and often hold no Wayland connection. XTest drivers
    run as X11 clients, and the protocol would need its own generator and
    review before any driver could use it.
  - The gateway's control channel. Only the browser viewer reaches it.
    Viewer receipts use it; on-box drivers would need the gateway's port
    and the viewer's authentication.
  - A network endpoint. Pass 2 keeps the endpoint for later, and every
    measured run was on the box.
  - Extending the portal. The RemoteDesktop portal is a standard interface,
    and its backends live in each desktop. A private extension to it would
    work only with waywire's backend.
- **Window query transport.** The window list is part of `InputDelivery1`
  (version 2): a `GetWindows` method and a `WindowsChanged` signal on the same
  session bus. Drivers already hold that connection for receipts, the list
  names windows by the receipts' id, and `GetWindows` reuses the receipts'
  barrier, so a driver can query after its input settles. Bounds are in output
  pixels, the pixels of the screenshots drivers already click from, with the
  scale to convert to portal coordinates. Rejected alternatives:
  - A separate D-Bus service or interface. It would need its own name, its own
    subscription and its own ordering with receipts; the barrier only holds
    when both come from one queue.
  - Adding geometry to wlr-foreign-toplevel-management or
    ext-foreign-toplevel-list through a private protocol extension. Drivers
    such as XTest loops hold no Wayland connection, and no released tool would
    read it.
  - Serving GNOME's `org.gnome.Shell.Introspect` or another desktop's API,
    which computer-use-linux already reads. That would pose as that desktop.
  - EWMH on the Xwayland root window. It would describe Wayland windows to X11
    clients only, and Xwayland already reports X11 windows' own geometry.
  - Per-window events instead of the whole list. A list of a few windows is a
    few hundred bytes, and one message with every window keeps stacking and
    geometry consistent with each other.
- **Window query privacy.** Every connection may call `GetWindows`, and every
  subscriber gets `WindowsChanged`. The list holds titles, app ids and pids
  that foreign-toplevel already gives every client, and bounds and stacking
  that screencopy already shows every client. It never carries window contents
  or input. Like the receipts, this assumes one trusted user per session.
- **Settle signal.** Settling is a method on `InputDelivery1` (version 3),
  `WaitSettled`, that replies once the output, or one window from
  `GetWindows`, has had no damage for a quiet window since an input, or with
  a distinct `timeout` result. The caller names the input by its receipt's
  serial, so a redraw before the call still counts, and picks the quiet
  window (default 300 ms, 10 ms to 10 s) and timeout (default 5 s, at most
  20 s, under the 25 s D-Bus call timeout). Damage is the output damage
  tracker's per-frame damage, which covers every client including Xwayland
  and the compositor's chrome after occlusion; the cursor is excluded because
  it is never in frames or screenshots. The reply follows every earlier
  signal, so it is also a barrier like `GetFocus`. Any connection may call
  it, but a receipt's serial only from the processes that receive that
  receipt. Rejected alternatives:
  - A `Settled` signal per input. The compositor would have to pick one quiet
    window for every consumer and time every input, including those nobody
    waits on, and a driver would still block until the signal arrived.
  - Settling inside `GetFocus` or `GetWindows`. Their replies are immediate
    barriers that drivers already call after input; a wait of hundreds of
    milliseconds would slow every caller that does not want it.
  - Waiting on client commits instead of output damage. A minimized,
    covered or off-screen window that animates would hold every wait open,
    although no screenshot shows it.
  - Wayland frame callbacks or presentation-time. A client sees them only for
    its own surfaces, and drivers such as XTest loops hold no Wayland
    connection.
  - Comparing screenshots in the driver. Each capture costs at least the raw
    copy (13 ms for grim) plus encoding, and two equal captures in a row only
    show that nothing changed between them.
  - A separate interface. Its replies could not follow the receipts it names,
    for the reason given under the window query transport.
- **Input delivery privacy.** A receipt reveals when a process sent each
  key and where it went, so the compositor sends it only to the input's
  owner: a subscriber whose process created the input source, or is one of
  its ancestors (the compositor records `/proc` ancestry when the source
  connects; for the portal, the source is the process that created the
  session). This lets a driver run a typing helper such as wtype and still
  see its receipts. XTest receipts go to every subscriber, because Xwayland
  does not identify the X client that used XTest, and every X client can
  already watch XTest input through XInput 2 raw events. Viewer receipts go
  only to the viewer that sent the input, over the control channel, never on
  the bus. Focus, window and text-field events go to every subscriber,
  because foreign-toplevel and the screen already show every client the same
  facts. A focus event names the receipt that caused it only for the owner
  of that input. Receipts never carry key codes, text or coordinates. Like
  the data-control decision, this assumes one trusted user per session; a
  sandboxed client under security-context-v1 should not reach the bus.
