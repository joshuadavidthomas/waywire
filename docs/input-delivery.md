# Input delivery receipts and focus events

Version 2. A driver that injects input learns, for each key, button, scroll and
text event, which window received it. It also learns when keyboard focus moves
and when windows appear, and it can list every window with its bounds on
screen, its place in the stacking order and its state. Any compositor that
routes seat input can implement this interface. waywire serves it.

The interface exists because input injection reports success when it sends
an event, not when a window receives it. When a dialog opens between a click
and the typing that follows, the typed text goes to the dialog, and the
driver call still succeeds.

## Transport

The interface is a D-Bus service on the session bus that the desktop gives to
its applications, the bus in `DBUS_SESSION_BUS_ADDRESS`. A driver that already
uses the RemoteDesktop portal or AT-SPI has a connection to that bus.

|                 |                                                       |
| --------------- | ----------------------------------------------------- |
| Well-known name | `io.github.joshuadavidthomas.waywire.InputDelivery1`  |
| Object path     | `/io/github/joshuadavidthomas/waywire/InputDelivery1` |
| Interface       | `io.github.joshuadavidthomas.waywire.InputDelivery1`  |

### Methods

- `Subscribe()`: start receiving signals on this connection. The compositor
  records the caller's process id with `GetConnectionUnixProcessID`. Calling
  it again has no effect.
- `Unsubscribe()`: stop receiving signals. A subscriber that disconnects is
  dropped automatically.
- `GetFocus() → a{sv}`: the window that holds keyboard focus now. Keys:
  `window`, `popup`, `text_field`, `text_purpose` and `serial`, with the
  meanings given below. The reply is sent after every signal for input the
  compositor processed before the call. A driver can therefore call
  `GetFocus` after its input as a barrier: when the reply arrives, all of the
  input's receipts have arrived.
- `GetWindows() → a{sv}`: every window, with its bounds on screen, stacking
  and state; see [Window list](#window-list). Since version 2. Like
  `GetFocus`, the reply follows every signal for earlier input, so it shows
  where windows are after that input. It needs no `Subscribe`.

### Properties

- `Version` (`u`): 2. Version 1 had no `GetWindows` and no `WindowsChanged`.

### Signals

The compositor sends each signal as a unicast signal to each subscriber that
may see it. Other connections on the bus do not receive it. Each signal has
one argument, an `a{sv}` dictionary, so later versions can add keys. Consumers
must ignore keys and signals they do not know.

- `Receipt`: one input event and where it went.
- `FocusChanged`: keyboard focus moved.
- `WindowMapped`: a window appeared.
- `TextFieldChanged`: the focused client gained or lost a focused text field.
- `WindowChanged`: a mapped window's title or app id changed.
- `WindowsChanged`: the window list changed. Since version 2.

## Records

### Window

A window is an `a{sv}` with these keys. An empty dictionary means no window.

| Key      | Type | Meaning                                                                                                                          |
| -------- | ---- | -------------------------------------------------------------------------------------------------------------------------------- |
| `id`     | `s`  | The window's `ext-foreign-toplevel-list-v1` identifier. Empty for windows without a handle, such as X11 override-redirect menus. |
| `app_id` | `s`  | Wayland app id, or the X11 `WM_CLASS` class.                                                                                     |
| `title`  | `s`  | Window title.                                                                                                                    |
| `pid`    | `u`  | The client's process id, or 0 when unknown.                                                                                      |
| `x11`    | `b`  | The window belongs to an X11 client through Xwayland.                                                                            |

### Receipt

| Key            | Type    | Meaning                                                                                                      |
| -------------- | ------- | ------------------------------------------------------------------------------------------------------------ |
| `serial`       | `t`     | Increases with each receipt and event. Shared by all signals.                                                |
| `time_usec`    | `t`     | `CLOCK_MONOTONIC` time, in microseconds.                                                                     |
| `source`       | `s`     | The input source; see below.                                                                                 |
| `source_pid`   | `u`     | The process that sent the input, or 0. For `xtest`, it is the Xwayland process.                              |
| `event`        | `s`     | `key`, `button`, `scroll` or `text`.                                                                         |
| `pressed`      | `b`     | For `key` and `button`: press or release.                                                                    |
| `button`       | `u`     | For `button`: the Linux button code, such as `0x110` (`BTN_LEFT`).                                           |
| `delivery`     | `s`     | `window`, `compositor`, `nowhere` or `dropped`; see below.                                                   |
| `window`       | `a{sv}` | The receiving window, or the window the compositor acted on.                                                 |
| `popup`        | `b`     | The event went to one of the window's popups or child surfaces, not its main surface.                        |
| `chrome`       | `s`     | For `compositor`: `titlebar`, `taskbar`, `window-menu` or `shortcut`.                                        |
| `text_field`   | `s`     | For `key` and `text` delivered to a window: `unknown`, `none` or `focused`.                                  |
| `text_purpose` | `s`     | For `focused`: the text-input content purpose, such as `normal`, `phone`, `email`, `password` or `terminal`. |

Receipts carry no key codes, keysyms, text or pointer coordinates.

The `source` values are:

| Value              | Input path                                                      |
| ------------------ | --------------------------------------------------------------- |
| `portal`           | RemoteDesktop portal `Notify*` methods                          |
| `portal-eis`       | An EI connection from the portal's `ConnectToEIS`               |
| `eis`              | An EI connection on `LIBEI_SOCKET`                              |
| `xtest`            | XTest requests that Xwayland forwards to the compositor over EI |
| `virtual-keyboard` | `zwp_virtual_keyboard_v1`                                       |
| `virtual-pointer`  | `zwlr_virtual_pointer_v1`                                       |
| `viewer`           | A remote viewer; never sent on the bus (see Privacy)            |

A compositor may add values for other input paths.

The `delivery` values are:

- `window`: a client surface received the event.
- `compositor`: the compositor consumed the event itself, for example a
  titlebar button, a taskbar click or a keyboard shortcut. The client did not
  receive it.
- `nowhere`: no surface received the event. Keyboard focus was empty, the
  pointer was over the bare desktop, or another source already held the same
  button.
- `dropped`: the compositor discarded the event because a person holds input
  control from the viewer.

The receipt describes where the event went when the compositor dispatched it.
For a key, that is the surface with keyboard focus at that moment. For a
button press, it is the surface under the pointer, or the surface that holds an
implicit grab.

### Text-field state

Clients that use `zwp_text_input_v3` enable text input when a text field gains
focus and disable it when the field loses focus. The compositor reports the
result:

- `focused`: the client has a text field focused. `text_purpose` gives its
  content purpose.
- `none`: the client uses text input and has no text field focused.
- `unknown`: the client has not enabled text input since it connected, so the
  compositor cannot tell. `unknown` does not mean that no text field is
  focused.

X11 clients are always `unknown`. Some clients send stale state: Qt 6 keeps
text input enabled after focus moves from a field to a button. Treat
`focused` and `none` as strong hints, not proof.

On waywire with Debian 13 packages, these clients enable text input by
default:

| Client                                                        | Text-field state                                        |
| ------------------------------------------------------------- | ------------------------------------------------------- |
| Chromium 156, Wayland, with or without `--enable-wayland-ime` | Reported; `none` when a page `alert()` takes focus      |
| Chromium 156 with `--wayland-text-input-version=1`            | `unknown`                                               |
| GTK 4                                                         | Reported                                                |
| GTK 3, Wayland                                                | Reported; focus moved by the program reports `none`     |
| Qt 6, Wayland                                                 | Reported; stays `focused` after focus moves to a button |
| foot                                                          | `focused`, purpose `terminal`                           |
| X11 clients through Xwayland (xterm, GTK and Qt on X11)       | `unknown`                                               |

### FocusChanged

| Key                   | Type    | Meaning                                                                                   |
| --------------------- | ------- | ----------------------------------------------------------------------------------------- |
| `serial`, `time_usec` | `t`     | As in a receipt.                                                                          |
| `window`              | `a{sv}` | The window that now holds keyboard focus. Empty when no window does.                      |
| `previous`            | `a{sv}` | The window that held focus before.                                                        |
| `popup`               | `b`     | Focus is on one of the window's popups.                                                   |
| `cause`               | `s`     | Why focus moved; see below.                                                               |
| `source`              | `s`     | When input caused the change: that input's source.                                        |
| `receipt`             | `t`     | When input caused the change and the subscriber may see that input's receipt: its serial. |

The `cause` values are:

| Value            | Meaning                                                                        |
| ---------------- | ------------------------------------------------------------------------------ |
| `new-window`     | A new top-level window mapped and took focus.                                  |
| `new-dialog`     | A new window with a parent mapped and took focus.                              |
| `activation`     | A client used a granted `xdg-activation-v1` token.                             |
| `window-request` | Another client asked: foreign-toplevel activation or X11 `_NET_ACTIVE_WINDOW`. |
| `window-closed`  | The focused window closed or unmapped.                                         |
| `minimized`      | The focused window was minimized.                                              |
| `popup`          | A popup took a keyboard grab.                                                  |
| `click`          | A button press.                                                                |
| `key`            | A key press, such as a shortcut.                                               |
| `other`          | Anything else.                                                                 |

When focus moves while the compositor dispatches an input event, the cause
comes from that input: a click on another window reports `click`. A dialog
that a click or key opens maps later, after the client responds, so its focus
change reports `new-dialog`.

### WindowMapped

| Key                   | Type    | Meaning                                                      |
| --------------------- | ------- | ------------------------------------------------------------ |
| `serial`, `time_usec` | `t`     | As in a receipt.                                             |
| `window`              | `a{sv}` | The new window.                                              |
| `parent`              | `a{sv}` | Its parent window, for dialogs. Empty for top-level windows. |
| `modal`               | `b`     | The window is a modal dialog.                                |

### TextFieldChanged

Sent when the client that holds keyboard focus enables or disables text input.

| Key                          | Type    | Meaning             |
| ---------------------------- | ------- | ------------------- |
| `serial`, `time_usec`        | `t`     | As in a receipt.    |
| `window`                     | `a{sv}` | The focused window. |
| `text_field`, `text_purpose` | `s`     | As in a receipt.    |

### WindowChanged

Sent when a window that has had `WindowMapped` changes its title or app id.

| Key                   | Type    | Meaning                                    |
| --------------------- | ------- | ------------------------------------------ |
| `serial`, `time_usec` | `t`     | As in a receipt.                           |
| `window`              | `a{sv}` | The window, with its new title and app id. |

## Window list

`GetWindows` returns, and `WindowsChanged` carries, the window list. The list
is read from the scene the compositor draws, so it matches the screen: after a
move, a resize, a client's own placement request, a raise or a minimize, the
next list shows the change before or with the frame that shows it.

| Key         | Type     | Meaning                                                                                                                 |
| ----------- | -------- | ----------------------------------------------------------------------------------------------------------------------- |
| `serial`    | `t`      | For `GetWindows`, the serial of the latest signal. For `WindowsChanged`, the signal's own serial.                       |
| `time_usec` | `t`      | `WindowsChanged` only: as in a receipt.                                                                                 |
| `width`     | `u`      | Output width in pixels.                                                                                                 |
| `height`    | `u`      | Output height in pixels.                                                                                                |
| `scale`     | `d`      | Output pixels per logical unit; see [Coordinates](#coordinates).                                                        |
| `work_area` | `(iiii)` | The part of the output where windows are placed, as `(x, y, width, height)`: the output above the compositor's taskbar. |
| `windows`   | `aa{sv}` | The windows, topmost first. Each is a [window](#window) record with the keys below.                                     |

Each entry in `windows` has the window record's keys (`id`, `app_id`, `title`,
`pid`, `x11`) and these:

| Key          | Type     | Meaning                                                                                                                                                     |
| ------------ | -------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `geometry`   | `(iiii)` | The client's content on screen, as `(x, y, width, height)` in output pixels. A click inside it reaches the client, unless a window above covers that point. |
| `frame`      | `(iiii)` | `geometry` plus the titlebar the compositor draws for the window, if any. Equal to `geometry` for windows that draw their own decorations.                  |
| `stack`      | `u`      | Position in the stacking order; 0 is the topmost window. Equal to the entry's index in `windows`.                                                           |
| `focused`    | `b`      | The window holds keyboard focus.                                                                                                                            |
| `minimized`  | `b`      | The window is minimized. It is not drawn, and its `geometry` is where it reappears when restored. A click there reaches whatever is below.                  |
| `maximized`  | `b`      | The window is maximized.                                                                                                                                    |
| `fullscreen` | `b`      | The window is fullscreen.                                                                                                                                   |
| `parent`     | `s`      | The `id` of the window this dialog belongs to. Empty for top-level windows.                                                                                 |
| `modal`      | `b`      | The window is a modal dialog.                                                                                                                               |

The list holds every window with an `id`: Wayland toplevels, including
dialogs and `window.open` popups, and managed X11 windows, including X11
dialogs. Menus, tooltips and other popups are not windows. They are part of
the window that opened them and are drawn above it, inside or outside its
`geometry`.

### Coordinates

`geometry`, `frame` and `work_area` are in output pixels with the origin at
the output's top-left corner: the pixels of a full-screen screenshot from grim,
`ext-image-copy-capture` or the Screenshot portal. X11 clients see the same
values through Xwayland, so `xdotool getwindowgeometry` agrees with `geometry`
for an X11 window. The RemoteDesktop portal's `NotifyPointerMotionAbsolute`
takes logical coordinates in its stream: divide by `scale`. At scale 1 the two
are the same.

To click a window, click inside `geometry` at a point that no window with a
lower `stack` covers. A dialog that belongs to the window is above it.

### WindowsChanged

Sent to every subscriber when the list differs from the one last sent: a
window appeared, closed, moved, resized, changed stacking, focus or state, or
changed its title or app id. The compositor compares lists once per pass of
its event loop, so a drag that moves a window sends one signal per pass, not
one per pointer event, and a change that is undone within one pass sends
nothing. The signal carries the whole list, like `GetWindows`, so a consumer
can keep the latest one and never call `GetWindows` again.

A driver that moves a window, or acts on input that may move one, calls
`GetWindows` after the input instead of sleeping. The reply arrives after any
`WindowsChanged` for the change, and shows where the windows are now. A window
that a client moves on its own after a delay, such as an application that
places a dialog once its contents load, moves later; `WindowsChanged`
announces it.

## Ordering

Serials increase in the order the compositor processes events. Signals are
sent in serial order, with one exception. A receipt's serial is allocated
when its input starts to dispatch, and focus events that the input causes are
sent before the receipt. Join them with the `receipt` key, not by arrival
order.

A window can take focus before it maps: Chromium activates a `window.open`
popup before its first buffer. The compositor reports that focus change after
the window's `WindowMapped`, when the window has an `id`, and keeps the cause
that moved focus, such as `activation`.

Every record is a snapshot, and titles often change after a window maps,
takes focus and focuses a text field. Chromium maps a page as
`Untitled - Chromium` and can enable text input before it sets the page
title; it maps a `window.open` popup as `about:blank`. `WindowChanged` reports
each later title, so join records by `id`, not by title.

## Privacy

A receipt says when a process sent each key and where it went. Over a shared
bus, that could leak another client's typing rhythm. The compositor therefore
limits who receives each signal:

- A subscriber receives a source's receipts only when the source's process is
  the subscriber or one of its descendants. For the portal, the source process
  is the process that created the portal session. The compositor records the
  process ancestry when the source connects.
- XTest receipts go to every subscriber. Any X client can already observe
  XTest input through XInput 2 raw events, and Xwayland does not say which X
  client sent it.
- Viewer receipts never go on the bus. Each one goes back to the viewer whose
  input it describes, over the gateway's control channel.
- `FocusChanged`, `WindowMapped`, `WindowChanged`, `TextFieldChanged` and
  `WindowsChanged` go to every subscriber, and any connection may call
  `GetWindows`. They describe window state that `ext-foreign-toplevel-list`,
  wlr-foreign-toplevel-management and the screen already show every client:
  screencopy shows where each window is. `receipt` is present only for the
  input's owners.

A driver must therefore subscribe from its own process, or from a process
that starts it, as the reference consumer `tools/receipts/receipts.py` does.

## Limits

A compositor sees surfaces, not widgets. A dialog drawn inside a window, such
as a browser's `alert()` or an in-window GTK popover, does not move keyboard
focus between surfaces. Such a dialog shows up in receipts only indirectly:
the window's text field changes to `none`, or the window's title changes. Use
AT-SPI focus events as the complement: `object:state-changed:focused` and
`window:activate` on the accessibility bus name the widget and the dialog
inside the window. Neither source covers everything. The compositor sees
windows of every client, including X11 clients and clients without
accessibility support. AT-SPI sees widgets inside clients that expose them.

## Viewer receipts

The browser viewer receives a receipt for each input event it sends, as an
`input-receipt` control message with the input's `sequence`, `delivery`,
`windowId`, `appId` and `title`. The SDK emits it as the session's `receipt`
event, and the viewer page dispatches it as a `waywire-input-receipt` DOM
event.

## Implementing elsewhere

A compositor implements the window list from the scene it renders: each
window's position in that scene, its content size, the titlebar it draws, and
the order it draws windows in. An X11 window manager can approximate it with
`_NET_CLIENT_LIST_STACKING` and `XTranslateCoordinates` on the root window.

A compositor implements receipts by recording, at each seat entry
point, the surface that receives the event and the source that sent it. It
emits focus events from its keyboard-focus hook and sets a cause around each
code path that moves focus. An X11 desktop can approximate it with XInput 2
focus events and the X server's XTest hooks, but it cannot attribute input to
a client. Keep the names, keys and value strings above so that one driver
works with every implementation.
