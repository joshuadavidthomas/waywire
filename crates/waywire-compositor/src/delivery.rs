//! Input delivery receipts and focus events, served on the private session bus
//! as described in `docs/input-delivery.md`.
//!
//! Every key, button, scroll and committed text event that reaches a seat
//! entry point yields one receipt naming its input source and where it went:
//! a window, the compositor's own chrome, nowhere, or dropped because a
//! browser holds input control. Keyboard focus changes, newly mapped windows
//! and text-field focus inside a window are reported as events.
//!
//! Receipts describe input, so they follow the input's owner. A bus
//! subscriber receives receipts only for sources whose process is the
//! subscriber or one of its descendants, or whose portal session it opened;
//! `XTest` input, which every X client can already observe, goes to every
//! subscriber. Browser input is never on the bus: its receipts go back to the
//! viewer that sent it, over the gateway's control channel. Receipts never
//! carry key codes or text. Focus and window events carry what
//! foreign-toplevel already tells every client, so every subscriber gets them.

mod bus;
pub(crate) mod settle;
mod windows;

use std::collections::HashMap;
use std::sync::mpsc;

use smithay::backend::input::ButtonState;
use smithay::backend::input::KeyState;
use smithay::desktop::Window;
use smithay::desktop::find_popup_root_surface;
use smithay::input::keyboard::KeyboardSource;
use smithay::reexports::wayland_protocols::wp::text_input::zv3::server::zwp_text_input_v3::ContentPurpose;
use smithay::reexports::wayland_server::Resource;
use smithay::reexports::wayland_server::backend::ClientId;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::wayland::compositor::get_parent;
use smithay::wayland::seat::WaylandFocus;
use smithay::wayland::text_input::TextInputSeat;
use smithay::xwayland::X11Surface;
use waywire_protocol::pipe;

pub(crate) use self::bus::start;
use self::settle::SerialKind;
use self::settle::Settle;
pub(crate) use self::windows::WindowList;
pub(crate) use self::windows::WindowsChanged;
use crate::State;
use crate::focus::KeyboardFocus;
use crate::focus::PointerFocus;

/// How an input source reaches the seat.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SourceKind {
    /// The browser viewer, through the gateway.
    Viewer,
    /// `RemoteDesktop` portal `Notify*` methods.
    Portal,
    /// An EI connection handed out by the portal's `ConnectToEIS`.
    PortalEis,
    /// An EI connection on the `LIBEI_SOCKET` socket.
    Eis,
    /// `XTest` requests that Xwayland forwards over EI.
    Xtest,
    /// `zwp_virtual_keyboard_v1`.
    VirtualKeyboard,
    /// `zwlr_virtual_pointer_v1`.
    VirtualPointer,
}

impl SourceKind {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Viewer => "viewer",
            Self::Portal => "portal",
            Self::PortalEis => "portal-eis",
            Self::Eis => "eis",
            Self::Xtest => "xtest",
            Self::VirtualKeyboard => "virtual-keyboard",
            Self::VirtualPointer => "virtual-pointer",
        }
    }
}

/// What the compositor knows about one input source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SourceInfo {
    pub(crate) kind: SourceKind,
    /// The process that sends the input, when the transport says.
    pub(crate) pid: Option<u32>,
    /// The process and its ancestors when the source appeared. A subscriber
    /// whose process is in this list owns the source's input.
    lineage: Vec<u32>,
}

impl SourceInfo {
    pub(crate) fn new(kind: SourceKind, pid: Option<u32>) -> Self {
        Self {
            kind,
            pid,
            lineage: pid.map(lineage).unwrap_or_default(),
        }
    }
}

/// `pid` followed by its ancestors, read from `/proc`.
fn lineage(pid: u32) -> Vec<u32> {
    let mut chain = vec![pid];
    let mut current = pid;
    // Bounded: a parent cycle cannot happen, but a racing reparent should
    // not loop forever either.
    for _ in 0..64 {
        let Some(parent) = std::fs::read_to_string(format!("/proc/{current}/stat"))
            .ok()
            .and_then(|stat| {
                stat.rsplit_once(')')?
                    .1
                    .split_whitespace()
                    .nth(1)?
                    .parse::<u32>()
                    .ok()
            })
        else {
            break;
        };
        if parent == 0 || chain.contains(&parent) {
            break;
        }
        chain.push(parent);
        current = parent;
    }
    chain
}

/// The kind of input a receipt describes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InputKind {
    Key { state: KeyState },
    Button { button: u32, state: ButtonState },
    Scroll,
    Text,
}

/// Part of the compositor's own interface that consumed input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Chrome {
    Titlebar,
    Taskbar,
    WindowMenu,
    Shortcut,
}

impl Chrome {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Titlebar => "titlebar",
            Self::Taskbar => "taskbar",
            Self::WindowMenu => "window-menu",
            Self::Shortcut => "shortcut",
        }
    }
}

/// Where one input event went, before it is described for a receipt.
#[derive(Clone, Debug)]
#[expect(
    clippy::large_enum_variant,
    reason = "one short-lived value per input event"
)]
pub(crate) enum Target {
    /// A client surface: the window, and which part of it the surface is.
    Client {
        window: Window,
        part: Part,
    },
    /// An X11 override-redirect window such as a menu, which has no
    /// foreign-toplevel identity.
    X11Popup(X11Surface),
    Compositor {
        window: Option<Window>,
        chrome: Chrome,
    },
    Nowhere,
    Dropped,
}

/// Which part of a window a surface is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Part {
    /// The window's main surface.
    Main,
    /// One of its popups or subsurfaces, or an X11 override-redirect window.
    Popup,
}

/// A window as receipts and events name it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct WindowRecord {
    /// The ext-foreign-toplevel-list identifier; empty for windows that have
    /// no foreign-toplevel handle.
    pub(crate) id: String,
    pub(crate) app_id: String,
    pub(crate) title: String,
    pub(crate) pid: Option<u32>,
    pub(crate) x11: bool,
}

/// Whether the client that holds keyboard focus has a text field focused,
/// as zwp-text-input-v3 reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TextField {
    /// The client never enabled text input, so the compositor cannot tell.
    Unknown,
    /// The client uses text input and has no text field focused now.
    None,
    /// A text field is focused; its content purpose.
    Focused(ContentPurpose),
}

impl TextField {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::None => "none",
            Self::Focused(_) => "focused",
        }
    }

    pub(crate) const fn purpose(self) -> Option<&'static str> {
        let Self::Focused(purpose) = self else {
            return None;
        };
        Some(match purpose {
            ContentPurpose::Alpha => "alpha",
            ContentPurpose::Digits => "digits",
            ContentPurpose::Number => "number",
            ContentPurpose::Phone => "phone",
            ContentPurpose::Url => "url",
            ContentPurpose::Email => "email",
            ContentPurpose::Name => "name",
            ContentPurpose::Password => "password",
            ContentPurpose::Pin => "pin",
            ContentPurpose::Date => "date",
            ContentPurpose::Time => "time",
            ContentPurpose::Datetime => "datetime",
            ContentPurpose::Terminal => "terminal",
            ContentPurpose::Normal | _ => "normal",
        })
    }
}

/// Why keyboard focus moved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FocusCause {
    NewWindow,
    NewDialog,
    /// A granted xdg-activation token.
    Activation,
    /// A window-management request from another client: foreign-toplevel
    /// activation or X11 `_NET_ACTIVE_WINDOW`.
    WindowRequest,
    /// The focused window closed, unmapped or died.
    WindowClosed,
    Minimized,
    /// A client's popup took a keyboard grab.
    Popup,
    Click,
    Key,
    /// Nothing the compositor attributes, such as a client's own request.
    Other,
}

impl FocusCause {
    const fn as_str(self) -> &'static str {
        match self {
            Self::NewWindow => "new-window",
            Self::NewDialog => "new-dialog",
            Self::Activation => "activation",
            Self::WindowRequest => "window-request",
            Self::WindowClosed => "window-closed",
            Self::Minimized => "minimized",
            Self::Popup => "popup",
            Self::Click => "click",
            Self::Key => "key",
            Self::Other => "other",
        }
    }
}

/// One receipt, as sent.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Receipt {
    pub(crate) serial: u64,
    pub(crate) time_usec: u64,
    pub(crate) source: SourceKind,
    pub(crate) source_pid: Option<u32>,
    pub(crate) input: InputKind,
    pub(crate) delivery: Destination,
}

/// One focus or window event, as sent.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Notice {
    FocusChanged {
        serial: u64,
        time_usec: u64,
        window: Option<(WindowRecord, Part)>,
        previous: Option<WindowRecord>,
        cause: FocusCause,
        source: Option<SourceKind>,
        receipt: Option<u64>,
    },
    WindowMapped {
        serial: u64,
        time_usec: u64,
        window: WindowRecord,
        parent: Option<WindowRecord>,
        modal: bool,
    },
    TextFieldChanged {
        serial: u64,
        time_usec: u64,
        window: Option<WindowRecord>,
        text_field: TextField,
    },
    /// A mapped window's title or app id changed.
    WindowChanged {
        serial: u64,
        time_usec: u64,
        window: WindowRecord,
    },
    /// The window list changed: a window moved, resized, restacked, changed
    /// state or metadata, appeared or went away.
    WindowsChanged(Box<WindowsChanged>),
}

/// A signal for the bus thread to send to each named connection.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Signal {
    Receipt(Receipt),
    Notice(Notice),
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Outgoing {
    pub(crate) to: Vec<String>,
    pub(crate) signal: Signal,
}

/// Work for the bus thread, in the order the loop produced it.
#[expect(
    clippy::large_enum_variant,
    reason = "flushes are rare; boxing every signal would cost more"
)]
pub(crate) enum Message {
    Send(Outgoing),
    /// Runs once every earlier signal has been written to the bus, so a
    /// method reply sent from it follows them.
    Flush(Box<dyn FnOnce() + Send>),
}

#[derive(Clone, Debug)]
struct Subscriber {
    name: String,
    pid: Option<u32>,
}

/// The input being dispatched, so receipts and focus events it causes can
/// name it.
#[derive(Clone, Copy, Debug)]
struct Dispatching {
    source: KeyboardSource,
    serial: u64,
    cause: FocusCause,
}

/// Receipt and focus-event state owned by the compositor loop.
#[derive(Default)]
pub(crate) struct Delivery {
    sources: HashMap<KeyboardSource, SourceInfo>,
    subscribers: Vec<Subscriber>,
    outgoing: Option<mpsc::Sender<Message>>,
    serial: u64,
    cause: Option<FocusCause>,
    dispatching: Option<Dispatching>,
    /// The focus last reported, so repeated `focus_changed` calls for the
    /// same target stay quiet; `None` before the first report.
    #[expect(
        clippy::option_option,
        reason = "no report yet differs from a report of no focus"
    )]
    reported_focus: Option<Option<(WindowRecord, Part)>>,
    /// The window list last sent in `WindowsChanged`.
    reported_windows: Option<WindowList>,
    /// Clients that have enabled text input at least once, and the content
    /// purpose of their latest activation.
    text_clients: HashMap<ClientId, ContentPurpose>,
    /// Why focus moved to a window that has not mapped yet. The change is
    /// reported when the window maps and has a foreign-toplevel identifier.
    deferred: Option<FocusCause>,
    /// Serial times, recent damage and `WaitSettled` calls in progress.
    settle: Settle,
}

/// Where one input went, as its receipt names it.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum Destination {
    /// A window's surface, with the text-field state of its client when
    /// the input is a key or committed text.
    Window {
        window: WindowRecord,
        part: Part,
        text_field: Option<TextField>,
    },
    /// The compositor's own interface, over `window` when there is one.
    Compositor {
        window: Option<WindowRecord>,
        chrome: Chrome,
    },
    Nowhere,
    /// Dropped because a browser holds input control.
    Dropped,
}

impl Destination {
    pub(crate) const fn as_str(&self) -> &'static str {
        match self {
            Self::Window { .. } => "window",
            Self::Compositor { .. } => "compositor",
            Self::Nowhere => "nowhere",
            Self::Dropped => "dropped",
        }
    }

    pub(crate) const fn window(&self) -> Option<&WindowRecord> {
        match self {
            Self::Window { window, .. } => Some(window),
            Self::Compositor { window, .. } => window.as_ref(),
            Self::Nowhere | Self::Dropped => None,
        }
    }

    pub(crate) const fn part(&self) -> Option<Part> {
        match self {
            Self::Window { part, .. } => Some(*part),
            Self::Compositor { .. } | Self::Nowhere | Self::Dropped => None,
        }
    }

    pub(crate) const fn chrome(&self) -> Option<Chrome> {
        match self {
            Self::Compositor { chrome, .. } => Some(*chrome),
            Self::Window { .. } | Self::Nowhere | Self::Dropped => None,
        }
    }

    pub(crate) const fn text_field(&self) -> Option<TextField> {
        match self {
            Self::Window { text_field, .. } => *text_field,
            Self::Compositor { .. } | Self::Nowhere | Self::Dropped => None,
        }
    }
}

/// Who receives one input's receipt.
#[derive(Clone, Debug)]
pub(crate) enum Recipients {
    /// The browser viewer that sent it, over the gateway.
    Viewer,
    /// Bus subscribers that own its source.
    Subscribers(Vec<String>),
}

impl Delivery {
    pub(crate) fn set_outgoing(&mut self, outgoing: mpsc::Sender<Message>) {
        self.outgoing = Some(outgoing);
    }

    pub(crate) fn describe(&mut self, source: KeyboardSource, info: SourceInfo) {
        self.sources.insert(source, info);
    }

    pub(crate) fn forget(&mut self, source: KeyboardSource) {
        self.sources.remove(&source);
    }

    pub(crate) fn source(&self, source: KeyboardSource) -> Option<&SourceInfo> {
        self.sources.get(&source)
    }

    pub(crate) fn subscribe(&mut self, name: String, pid: Option<u32>) {
        self.subscribers
            .retain(|subscriber| subscriber.name != name);
        self.subscribers.push(Subscriber { name, pid });
    }

    pub(crate) fn unsubscribe(&mut self, name: &str) {
        self.subscribers
            .retain(|subscriber| subscriber.name != name);
    }

    /// Whether anyone receives focus and window events, so the loop can
    /// skip describing windows nobody asked about.
    pub(crate) fn has_subscribers(&self) -> bool {
        !self.subscribers.is_empty()
    }

    pub(crate) const fn last_serial(&self) -> u64 {
        self.serial
    }

    fn next_serial(&mut self) -> u64 {
        self.issue(settle::Audience::Everyone, SerialKind::Notice)
    }

    /// Allocate a serial of `kind` that `audience` may name in
    /// `WaitSettled`.
    fn issue(&mut self, audience: settle::Audience, kind: SerialKind) -> u64 {
        self.serial += 1;
        self.settle.issue(self.serial, audience, kind);
        self.serial
    }

    /// The kind of `source`; the browser's source is always the viewer.
    fn kind(&self, source: KeyboardSource) -> Option<SourceKind> {
        if source == KeyboardSource::MAIN {
            return Some(SourceKind::Viewer);
        }
        self.sources.get(&source).map(|info| info.kind)
    }

    /// Whether `subscriber` may see `source`'s receipts.
    fn owns(&self, subscriber: &Subscriber, source: KeyboardSource) -> bool {
        let Some(info) = self.sources.get(&source) else {
            return false;
        };
        match info.kind {
            SourceKind::Viewer => false,
            SourceKind::Xtest => true,
            SourceKind::Portal
            | SourceKind::PortalEis
            | SourceKind::Eis
            | SourceKind::VirtualKeyboard
            | SourceKind::VirtualPointer => subscriber
                .pid
                .is_some_and(|pid| info.lineage.contains(&pid)),
        }
    }

    /// Subscribers that may see `source`'s receipts.
    fn owners(&self, source: KeyboardSource) -> Vec<String> {
        self.subscribers
            .iter()
            .filter(|subscriber| self.owns(subscriber, source))
            .map(|subscriber| subscriber.name.clone())
            .collect()
    }

    /// Whether anyone receives `source`'s receipts, so the loop can skip
    /// describing input nobody asked about.
    pub(crate) fn wanted(&self, source: KeyboardSource) -> bool {
        source == KeyboardSource::MAIN
            || self
                .subscribers
                .iter()
                .any(|subscriber| self.owns(subscriber, source))
    }

    fn send(&self, to: Vec<String>, signal: Signal) {
        if to.is_empty() {
            return;
        }
        if let Some(outgoing) = &self.outgoing
            && outgoing
                .send(Message::Send(Outgoing { to, signal }))
                .is_err()
        {
            tracing::debug!("input delivery bus thread is gone");
        }
    }

    /// Run `after` once every signal sent so far is on the bus.
    pub(crate) fn flush(&self, after: Box<dyn FnOnce() + Send>) {
        match &self.outgoing {
            Some(outgoing) => {
                if let Err(mpsc::SendError(Message::Flush(after))) =
                    outgoing.send(Message::Flush(after))
                {
                    after();
                }
            }
            None => after(),
        }
    }

    fn everyone(&self) -> Vec<String> {
        self.subscribers
            .iter()
            .map(|subscriber| subscriber.name.clone())
            .collect()
    }

    /// Make `cause` the reason for keyboard focus changes that no input
    /// being dispatched explains, and return the reason it replaces.
    pub(crate) const fn replace_cause(&mut self, cause: Option<FocusCause>) -> Option<FocusCause> {
        std::mem::replace(&mut self.cause, cause)
    }

    /// Start dispatching one input event and return its receipt serial.
    /// Focus changes until [`Self::finish_input`] name this input.
    pub(crate) fn begin_input(&mut self, source: KeyboardSource, cause: FocusCause) -> u64 {
        let lineage = self
            .sources
            .get(&source)
            .map_or(&[][..], |info| &info.lineage);
        let audience = settle::Audience::of(self.kind(source), lineage);
        let serial = self.issue(audience, SerialKind::Input);
        self.dispatching = Some(Dispatching {
            source,
            serial,
            cause,
        });
        serial
    }

    /// End the input started by [`Self::begin_input`] and say who receives
    /// its receipt; `None` when nobody does, so it need not be described.
    pub(crate) fn finish_input(&mut self, source: KeyboardSource) -> Option<Recipients> {
        self.dispatching = None;
        if source == KeyboardSource::MAIN {
            return Some(Recipients::Viewer);
        }
        let owners = self.owners(source);
        (!owners.is_empty()).then_some(Recipients::Subscribers(owners))
    }

    /// The receipt for input `serial` from `source` that went to
    /// `delivery`.
    pub(crate) fn receipt(
        &self,
        source: KeyboardSource,
        serial: u64,
        input: InputKind,
        delivery: Destination,
    ) -> Receipt {
        Receipt {
            serial,
            time_usec: monotonic_usec(),
            source: self.kind(source).unwrap_or(SourceKind::Viewer),
            source_pid: self.source(source).and_then(|info| info.pid),
            input,
            delivery,
        }
    }

    /// Send a receipt to the subscribers [`Self::finish_input`] named.
    pub(crate) fn send_receipt(&self, owners: Vec<String>, receipt: Receipt) {
        self.send(owners, Signal::Receipt(receipt));
    }

    /// Why focus is moving now, and the input that moves it.
    fn focus_cause(&self) -> (FocusCause, Option<Dispatching>) {
        // Input being dispatched explains the change best: a click on a
        // taskbar chip that minimizes is the agent's click, not a minimize.
        match (self.dispatching, self.cause) {
            (Some(input), _) => (input.cause, Some(input)),
            (None, Some(cause)) => (cause, None),
            (None, None) => (FocusCause::Other, None),
        }
    }

    /// Keyboard focus moved to a toplevel that has not mapped yet, so it
    /// has no identifier; [`Self::take_deferred`] reports it once it maps.
    pub(crate) fn defer_focus(&mut self) {
        self.deferred = Some(self.focus_cause().0);
    }

    /// Why focus moved to a window before it mapped, if it did.
    pub(crate) const fn take_deferred(&mut self) -> Option<FocusCause> {
        self.deferred.take()
    }

    /// Keyboard focus moved to a part of a window, or to none. Reported
    /// unless it is the focus last reported.
    pub(crate) fn focus_changed(&mut self, focus: Option<(WindowRecord, Part)>) {
        self.deferred = None;
        if self.reported_focus.as_ref() == Some(&focus) {
            return;
        }
        let previous = self
            .reported_focus
            .replace(focus)
            .flatten()
            .map(|(window, _)| window);
        let everyone = self.everyone();
        if everyone.is_empty() {
            return;
        }
        let (cause, input) = self.focus_cause();
        let source = input.and_then(|input| self.kind(input.source));
        let serial = self.next_serial();
        let focus = self.reported_focus.as_ref().and_then(Option::as_ref);
        for name in everyone {
            // Only the input's owners learn which of their receipts it was.
            let owned = input.is_some_and(|input| self.owners(input.source).contains(&name));
            let notice = Notice::FocusChanged {
                serial,
                time_usec: monotonic_usec(),
                window: focus.cloned(),
                previous: previous.clone(),
                cause,
                source,
                receipt: owned.then(|| input.map(|input| input.serial)).flatten(),
            };
            self.send(vec![name], Signal::Notice(notice));
        }
    }

    /// `client` enabled text input for a field with `purpose`. Clients
    /// that `alive` says are gone are forgotten.
    pub(crate) fn text_input_enabled(
        &mut self,
        client: ClientId,
        purpose: ContentPurpose,
        alive: impl Fn(&ClientId) -> bool,
    ) {
        self.text_clients.retain(|id, _| alive(id));
        self.text_clients.insert(client, purpose);
    }

    /// The text-field state of `client`, which holds keyboard focus;
    /// `active` says whether it has a text input enabled now.
    pub(crate) fn text_field(&self, client: &ClientId, active: impl FnOnce() -> bool) -> TextField {
        let Some(purpose) = self.text_clients.get(client) else {
            return TextField::Unknown;
        };
        if active() {
            TextField::Focused(*purpose)
        } else {
            TextField::None
        }
    }

    /// The focused client's text field changed to `text_field`.
    pub(crate) fn text_field_changed(
        &mut self,
        window: Option<WindowRecord>,
        text_field: TextField,
    ) {
        let everyone = self.everyone();
        if everyone.is_empty() {
            return;
        }
        let notice = Notice::TextFieldChanged {
            serial: self.next_serial(),
            time_usec: monotonic_usec(),
            window,
            text_field,
        };
        self.send(everyone, Signal::Notice(notice));
    }

    /// A window became visible for the first time.
    pub(crate) fn window_mapped(
        &mut self,
        window: WindowRecord,
        parent: Option<WindowRecord>,
        modal: bool,
    ) {
        let everyone = self.everyone();
        if everyone.is_empty() {
            return;
        }
        let notice = Notice::WindowMapped {
            serial: self.next_serial(),
            time_usec: monotonic_usec(),
            window,
            parent,
            modal,
        };
        self.send(everyone, Signal::Notice(notice));
    }

    /// A mapped window's title or app id changed.
    pub(crate) fn window_changed(&mut self, window: WindowRecord) {
        let everyone = self.everyone();
        if everyone.is_empty() {
            return;
        }
        let notice = Notice::WindowChanged {
            serial: self.next_serial(),
            time_usec: monotonic_usec(),
            window,
        };
        self.send(everyone, Signal::Notice(notice));
    }

    /// Whether anyone receives `WindowsChanged`. With nobody, the list last
    /// sent is forgotten, so the next subscriber gets the whole list.
    pub(crate) fn wants_window_list(&mut self) -> bool {
        if self.subscribers.is_empty() {
            self.reported_windows = None;
            return false;
        }
        true
    }

    /// Send `WindowsChanged` with `list` when it differs from the one last
    /// sent.
    pub(crate) fn windows_listed(&mut self, list: WindowList) {
        if self.reported_windows.as_ref() == Some(&list) {
            return;
        }
        self.reported_windows = Some(list.clone());
        let changed = WindowsChanged {
            serial: self.next_serial(),
            time_usec: monotonic_usec(),
            list,
        };
        self.send(
            self.everyone(),
            Signal::Notice(Notice::WindowsChanged(Box::new(changed))),
        );
    }
}

impl Receipt {
    /// The receipt as the viewer that sent the input with `sequence` gets it.
    pub(crate) fn for_viewer(&self, sequence: pipe::InputSequence) -> pipe::InputReceipt {
        let none = WindowRecord::default();
        let window = self.delivery.window().unwrap_or(&none);
        pipe::InputReceipt {
            sequence,
            delivery: match self.delivery {
                Destination::Window { .. } => pipe::ReceiptDelivery::Window,
                Destination::Compositor { .. } => pipe::ReceiptDelivery::Compositor,
                Destination::Nowhere => pipe::ReceiptDelivery::Nowhere,
                Destination::Dropped => pipe::ReceiptDelivery::Dropped,
            },
            window_id: pipe::ReceiptText::truncated(&window.id),
            app_id: pipe::ReceiptText::truncated(&window.app_id),
            title: pipe::ReceiptText::truncated(&window.title),
        }
    }
}

pub(crate) fn monotonic_usec() -> u64 {
    nix::time::clock_gettime(nix::time::ClockId::CLOCK_MONOTONIC).map_or(0, |time| {
        u64::try_from(time.tv_sec()).unwrap_or(0) * 1_000_000
            + u64::try_from(time.tv_nsec()).unwrap_or(0) / 1000
    })
}

/// How receipts and events name an X11 window.
pub(crate) fn x11_record(surface: &X11Surface) -> WindowRecord {
    WindowRecord {
        id: String::new(),
        app_id: surface.class(),
        title: surface.title(),
        pid: surface.pid(),
        x11: true,
    }
}

impl State {
    /// Run `change` with `cause` as the reason for any keyboard focus change
    /// it makes that no input being dispatched explains.
    pub(crate) fn with_focus_cause<R>(
        &mut self,
        cause: FocusCause,
        change: impl FnOnce(&mut Self) -> R,
    ) -> R {
        let previous = self.delivery.replace_cause(Some(cause));
        let result = change(self);
        self.delivery.replace_cause(previous);
        result
    }

    /// Report where the input started by [`Delivery::begin_input`] went.
    pub(crate) fn finish_input(
        &mut self,
        source: KeyboardSource,
        serial: u64,
        input: InputKind,
        target: &Target,
        text_field: Option<TextField>,
    ) {
        let Some(recipients) = self.delivery.finish_input(source) else {
            return;
        };
        let destination = match target {
            Target::Client { window, part } => Destination::Window {
                window: self.window_record(window),
                part: *part,
                text_field,
            },
            Target::X11Popup(surface) => Destination::Window {
                window: x11_record(surface),
                part: Part::Popup,
                text_field,
            },
            Target::Compositor { window, chrome } => Destination::Compositor {
                window: window.as_ref().map(|window| self.window_record(window)),
                chrome: *chrome,
            },
            Target::Nowhere => Destination::Nowhere,
            Target::Dropped => Destination::Dropped,
        };
        let receipt = self.delivery.receipt(source, serial, input, destination);
        match recipients {
            Recipients::Viewer => {
                if let Some(sequence) = self.stream.latest_input_sequence() {
                    self.emit(pipe::Event::InputReceipt(receipt.for_viewer(sequence)));
                }
            }
            Recipients::Subscribers(owners) => self.delivery.send_receipt(owners, receipt),
        }
    }

    /// Where a key pressed now goes, with the focused client's text field.
    pub(crate) fn keyboard_target(&self) -> (Target, TextField) {
        let Some(focus) = self
            .seat
            .get_keyboard()
            .and_then(|keyboard| keyboard.current_focus())
        else {
            return (Target::Nowhere, TextField::Unknown);
        };
        let target = match &focus {
            KeyboardFocus::Wayland(surface) => self.surface_target(surface),
            KeyboardFocus::X11(surface) => self.x11_target(surface),
        };
        (target, self.text_field())
    }

    /// Where a button pressed now goes, given the pointer's focus.
    pub(crate) fn pointer_target(&self) -> Target {
        match self
            .seat
            .get_pointer()
            .and_then(|pointer| pointer.current_focus())
        {
            Some(PointerFocus::Wayland(surface)) => self.surface_target(&surface),
            Some(PointerFocus::X11(surface)) => self.x11_target(&surface),
            None => Target::Nowhere,
        }
    }

    fn x11_target(&self, surface: &X11Surface) -> Target {
        if surface.is_override_redirect() {
            return Target::X11Popup(surface.clone());
        }
        self.space
            .elements()
            .find(|window| window.x11_surface() == Some(surface))
            .map_or(Target::Nowhere, |window| Target::Client {
                window: window.clone(),
                part: Part::Main,
            })
    }

    /// The window a Wayland surface belongs to: its own toplevel, the
    /// toplevel of the popup it is part of, or an X11 window's surface.
    fn surface_target(&self, surface: &WlSurface) -> Target {
        let mut root = surface.clone();
        let mut child = false;
        while let Some(parent) = get_parent(&root) {
            root = parent;
            child = true;
        }
        if let Some(popup) = self.popups.find_popup(&root)
            && let Ok(top) = find_popup_root_surface(&popup)
        {
            root = top;
            child = true;
        }
        self.space
            .elements()
            .find(|window| window.wl_surface().is_some_and(|own| *own == root))
            .map_or(Target::Nowhere, |window| {
                if let Some(x11) = window.x11_surface()
                    && x11.is_override_redirect()
                {
                    return Target::X11Popup(x11.clone());
                }
                Target::Client {
                    window: window.clone(),
                    part: if child { Part::Popup } else { Part::Main },
                }
            })
    }

    pub(crate) fn window_record(&self, window: &Window) -> WindowRecord {
        if let Some(surface) = window.x11_surface()
            && surface.is_override_redirect()
        {
            return x11_record(surface);
        }
        let (title, app_id) = crate::toplevel_list::metadata(window).unwrap_or_default();
        WindowRecord {
            id: self.toplevel_list.identifier(window).unwrap_or_default(),
            app_id,
            title,
            pid: self.window_pid(window),
            x11: window.x11_surface().is_some(),
        }
    }

    /// The text-field state of the client that holds keyboard focus.
    pub(crate) fn text_field(&self) -> TextField {
        let text_input = self.seat.text_input();
        let Some(client) = text_input.focus().and_then(|surface| surface.client()) else {
            return TextField::Unknown;
        };
        self.delivery.text_field(&client.id(), || {
            let mut active = false;
            text_input.with_active_text_input(|_, _| active = true);
            active
        })
    }

    /// The client holding keyboard focus enabled or disabled text input.
    pub(crate) fn text_input_changed(&mut self, purpose: Option<ContentPurpose>) {
        let surface = self.seat.text_input().focus();
        if let (Some(purpose), Some(client)) =
            (purpose, surface.as_ref().and_then(Resource::client))
        {
            let handle = &self.display_handle;
            self.delivery
                .text_input_enabled(client.id(), purpose, |id| {
                    handle.backend_handle().get_client_data(id.clone()).is_ok()
                });
        }
        if !self.delivery.has_subscribers() {
            return;
        }
        let window = surface
            .and_then(|surface| self.focused_part(self.surface_target(&surface)))
            .map(|(window, _)| window);
        let text_field = self.text_field();
        self.delivery.text_field_changed(window, text_field);
    }

    /// Keyboard focus moved to `focus`. Called from the seat's focus hook.
    pub(crate) fn keyboard_focus_changed(&mut self, focus: Option<&KeyboardFocus>) {
        let target = match focus {
            Some(KeyboardFocus::Wayland(surface)) => self.surface_target(surface),
            Some(KeyboardFocus::X11(surface)) => self.x11_target(surface),
            None => Target::Nowhere,
        };
        // Clients can activate a toplevel before its first buffer, when it
        // has no identifier yet; report it once it maps.
        if let Target::Client { window, .. } = &target
            && window.toplevel().is_some()
            && !self.windows.get(window).is_some_and(|state| state.mapped)
        {
            self.delivery.defer_focus();
            return;
        }
        let focus = self.focused_part(target);
        self.delivery.focus_changed(focus);
    }

    /// The window and part of it that `target` names, for focus reports;
    /// `None` for anything but a window.
    fn focused_part(&self, target: Target) -> Option<(WindowRecord, Part)> {
        match target {
            Target::Client { window, part } => Some((self.window_record(&window), part)),
            Target::X11Popup(surface) => Some((x11_record(&surface), Part::Popup)),
            Target::Compositor { .. } | Target::Nowhere | Target::Dropped => None,
        }
    }

    /// Describe a Wayland client's input device for receipts.
    pub(crate) fn describe_client_source(
        &mut self,
        source: KeyboardSource,
        kind: SourceKind,
        client: Option<smithay::reexports::wayland_server::Client>,
    ) {
        let pid = client
            .and_then(|client| client.get_credentials(&self.display_handle).ok())
            .and_then(|credentials| u32::try_from(credentials.pid).ok());
        self.delivery.describe(source, SourceInfo::new(kind, pid));
    }

    /// Give a newly mapped window keyboard focus and report the change,
    /// including a change made before it mapped.
    pub(crate) fn focus_mapped_window(&mut self, window: &Window, cause: FocusCause) {
        let cause = self.delivery.take_deferred().unwrap_or(cause);
        self.with_focus_cause(cause, |state| {
            state.focus_window(Some(window.clone()));
            let focus = state
                .seat
                .get_keyboard()
                .and_then(|keyboard| keyboard.current_focus());
            state.keyboard_focus_changed(focus.as_ref());
        });
    }

    /// A window became visible for the first time.
    pub(crate) fn window_mapped(&mut self, window: &Window) {
        if !self.delivery.has_subscribers() {
            return;
        }
        let parent =
            crate::wm::parent_window(&self.space, window).map(|parent| self.window_record(&parent));
        let record = self.window_record(window);
        self.delivery
            .window_mapped(record, parent, crate::wm::modal_dialog(window));
    }

    /// A mapped window's title or app id changed. Clients often set the
    /// final title after they map and take focus, so the records in earlier
    /// events are stale until this arrives.
    pub(crate) fn window_changed(&mut self, window: &Window) {
        if !self.delivery.has_subscribers() {
            return;
        }
        let record = self.window_record(window);
        self.delivery.window_changed(record);
    }

    /// The current focus, for the bus's `GetFocus` method.
    pub(crate) fn focus_report(&self) -> (Option<(WindowRecord, Part)>, TextField) {
        let (target, text_field) = self.keyboard_target();
        match self.focused_part(target) {
            Some(focus) => (Some(focus), text_field),
            None => (None, TextField::Unknown),
        }
    }
}
