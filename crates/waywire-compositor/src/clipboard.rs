//! The desktop clipboard and primary selection, bridged between Wayland and
//! X11. Clipboard text also syncs with the browser viewer and outlives the app
//! that copied it; the primary selection stays on the desktop and ends with its
//! owner. The compositor converts between UTF-8 types and X11 `STRING`
//! (ISO 8859-1) for clients that only know one of them.
//!
//! Data-control clients (`ext-data-control-v1` and the wlr protocol that
//! distribution wl-clipboard builds speak) read and set both selections
//! without keyboard focus. Smithay routes their sources and offers through the
//! same seat selections and `SelectionHandler` calls as focused clients, so the
//! X11 bridge, viewer sync and kept text treat them like any Wayland owner.
//!
//! Transfers through the compositor are nonblocking, bounded by
//! `MAX_CLIPBOARD_BYTES` and a timeout, and logged when dropped. Replacing an
//! offer drops its read FD, so an obsolete producer can never overwrite a newer
//! browser/client selection.
use std::fs::File;
use std::io::Read as _;
use std::io::Write as _;
use std::os::fd::OwnedFd;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use std::time::Instant;

use nix::fcntl::FcntlArg;
use nix::fcntl::OFlag;
use nix::fcntl::fcntl;
use smithay::input::Seat;
use smithay::wayland::selection::SelectionHandler;
use smithay::wayland::selection::SelectionSource;
use smithay::wayland::selection::SelectionTarget;
use smithay::wayland::selection::data_device;
use smithay::wayland::selection::data_device::clear_data_device_selection;
use smithay::wayland::selection::data_device::current_data_device_selection_userdata;
use smithay::wayland::selection::data_device::request_data_device_client_selection;
use smithay::wayland::selection::data_device::set_data_device_selection;
use smithay::wayland::selection::ext_data_control;
use smithay::wayland::selection::primary_selection;
use smithay::wayland::selection::primary_selection::PrimarySelectionHandler;
use smithay::wayland::selection::primary_selection::PrimarySelectionState;
use smithay::wayland::selection::primary_selection::clear_primary_selection;
use smithay::wayland::selection::primary_selection::current_primary_selection_userdata;
use smithay::wayland::selection::primary_selection::request_primary_client_selection;
use smithay::wayland::selection::primary_selection::set_primary_selection;
use smithay::wayland::selection::wlr_data_control;
use waywire_protocol::pipe::ClipboardText;
use waywire_protocol::pipe::Event;
use waywire_protocol::pipe::MAX_CLIPBOARD_BYTES;

use super::State;

const MIME: &str = "text/plain;charset=utf-8";
/// Text types, in the order the compositor reads them.
const TEXT: [&str; 4] = [MIME, "UTF8_STRING", "STRING", "text/plain"];
/// Text types the compositor offers to Wayland clients.
const WAYLAND_TEXT: [&str; 2] = [MIME, "text/plain"];
/// Text types offered to X11 clients: Smithay's XWM maps the first two to
/// `UTF8_STRING` and `TEXT`; `STRING` is the X11 atom's own name.
const X11_TEXT: [&str; 3] = [MIME, "text/plain", "STRING"];
const TIMEOUT: Duration = Duration::from_secs(3);
/// Transfers of each kind the compositor relays at once.
const MAX_TRANSFERS: usize = 8;

#[derive(Clone)]
pub(crate) enum SelectionData {
    /// Text the compositor serves itself: browser writes and kept clipboards.
    Text(Arc<str>),
    X11,
}

/// The client owning a selection and the types it offers.
#[derive(Default)]
struct Owner {
    types: Vec<String>,
    wayland: bool,
}

struct Read {
    fd: File,
    bytes: Vec<u8>,
    latin1: bool,
    deadline: Instant,
    purpose: &'static str,
}

enum Progress {
    Pending,
    Done(String),
    Failed,
}

struct Write {
    fd: File,
    bytes: Arc<[u8]>,
    offset: usize,
    deadline: Instant,
}

pub(super) struct Clipboard {
    primary_state: PrimarySelectionState,
    ext_data_control: ext_data_control::DataControlState,
    wlr_data_control: wlr_data_control::DataControlState,
    /// Owners of the clipboard and the primary selection.
    owners: [Owner; 2],
    pending_offer: Option<Vec<String>>,
    /// The viewer's read of the current clipboard selection.
    read: Option<Read>,
    /// Reads converted into another encoding for a requesting client.
    conversions: Vec<(Read, OwnedFd, bool)>,
    writes: Vec<Write>,
    /// Text of the current clipboard selection, kept after its owner exits.
    kept: Option<Arc<str>>,
    /// The clipboard owner exited; keep its text once the read finishes.
    orphaned: bool,
    /// Set when a Wayland client disconnects. Smithay drops a dead client's
    /// selection without calling `new_selection`, so the next tick checks.
    /// Each compositor owns its flag, so one compositor's tick never consumes
    /// another's disconnect.
    client_left: Arc<AtomicBool>,
}

impl Clipboard {
    pub(super) fn new(display: &smithay::reexports::wayland_server::DisplayHandle) -> Self {
        let primary_state = PrimarySelectionState::new::<State>(display);
        // Every client may use data control: the session is a trusted single
        // user's, like screencopy and the virtual input protocols.
        let ext_data_control = ext_data_control::DataControlState::new::<State, _>(
            display,
            Some(&primary_state),
            |_| true,
        );
        let wlr_data_control = wlr_data_control::DataControlState::new::<State, _>(
            display,
            Some(&primary_state),
            |_| true,
        );
        Self {
            primary_state,
            ext_data_control,
            wlr_data_control,
            owners: [Owner::default(), Owner::default()],
            pending_offer: None,
            read: None,
            conversions: Vec::new(),
            writes: Vec::new(),
            kept: None,
            orphaned: false,
            client_left: Arc::default(),
        }
    }

    /// The flag each client sets when it disconnects.
    pub(crate) fn client_left(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.client_left)
    }

    fn owner(&mut self, target: SelectionTarget) -> &mut Owner {
        match target {
            SelectionTarget::Clipboard => &mut self.owners[0],
            SelectionTarget::Primary => &mut self.owners[1],
        }
    }
}

/// X11 `STRING` is ISO 8859-1; every other text type is UTF-8.
fn latin1(mime: &str) -> bool {
    mime == "STRING"
}

fn decode(bytes: Vec<u8>, latin1: bool) -> String {
    if latin1 {
        return bytes.into_iter().map(char::from).collect();
    }
    String::from_utf8(bytes).unwrap_or_else(|error| {
        tracing::warn!("selection text is not valid UTF-8; replacing invalid bytes");
        String::from_utf8_lossy(error.as_bytes()).into_owned()
    })
}

fn encode(text: &str, latin1: bool) -> Arc<[u8]> {
    if latin1 {
        text.chars()
            .map(|c| u8::try_from(c).unwrap_or(b'?'))
            .collect()
    } else {
        Arc::from(text.as_bytes())
    }
}

/// `types` plus each of `extra` it lacks, when it offers any text type.
fn with_text(mut types: Vec<String>, extra: &[&str]) -> Vec<String> {
    if types.iter().any(|t| TEXT.contains(&t.as_str())) {
        for mime in extra {
            if !types.iter().any(|t| t == mime) {
                types.push((*mime).into());
            }
        }
    }
    types
}

fn strings(types: &[&str]) -> Vec<String> {
    types.iter().map(|&t| t.into()).collect()
}

fn nonblocking_pipe() -> anyhow::Result<(File, OwnedFd)> {
    let (read, write) = nix::unistd::pipe2(OFlag::O_CLOEXEC)?;
    let flags = OFlag::from_bits_truncate(fcntl(&read, FcntlArg::F_GETFL)?);
    fcntl(&read, FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK))?;
    Ok((File::from(read), write))
}

impl Read {
    fn new(fd: File, latin1: bool, purpose: &'static str) -> Self {
        Self {
            fd,
            bytes: Vec::new(),
            latin1,
            deadline: Instant::now() + TIMEOUT,
            purpose,
        }
    }

    /// Reads what the owner has written so far. Oversized, stalled and failed
    /// transfers are logged and dropped, which closes the owner's pipe.
    fn poll(&mut self) -> Progress {
        if Instant::now() >= self.deadline {
            tracing::warn!(
                purpose = self.purpose,
                bytes = self.bytes.len(),
                "selection owner did not finish writing within {TIMEOUT:?}; dropped the transfer"
            );
            return Progress::Failed;
        }
        let mut part = vec![0; 65536];
        match self.fd.read(&mut part) {
            Ok(0) => Progress::Done(decode(std::mem::take(&mut self.bytes), self.latin1)),
            Ok(n) if self.bytes.len() + n <= MAX_CLIPBOARD_BYTES => {
                self.bytes.extend_from_slice(&part[..n]);
                Progress::Pending
            }
            Ok(n) => {
                tracing::warn!(
                    purpose = self.purpose,
                    bytes = self.bytes.len() + n,
                    limit = MAX_CLIPBOARD_BYTES,
                    "selection text exceeds the transfer limit; dropped the transfer"
                );
                Progress::Failed
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                ) =>
            {
                Progress::Pending
            }
            Err(error) => {
                tracing::warn!(purpose = self.purpose, %error, "selection read failed");
                Progress::Failed
            }
        }
    }
}

impl SelectionHandler for State {
    type SelectionUserData = SelectionData;
    fn new_selection(
        &mut self,
        target: SelectionTarget,
        source: Option<SelectionSource>,
        _: Seat<Self>,
    ) {
        // Smithay invokes this before it installs the seat's selection.
        let types = source.map(|source| source.mime_types());
        if let Some(xwm) = &mut self.xwm
            && let Err(error) = xwm.new_selection(
                target,
                types.clone().map(|types| with_text(types, &X11_TEXT)),
            )
        {
            tracing::warn!(%error, ?target, "forward selection ownership to X11");
        }
        self.x11_stacking = None;
        *self.clipboard.owner(target) = Owner {
            wayland: types.is_some(),
            types: types.clone().unwrap_or_default(),
        };
        if target == SelectionTarget::Clipboard {
            self.clipboard_offer(types.unwrap_or_default());
        }
    }
    fn send_selection(
        &mut self,
        target: SelectionTarget,
        mime: String,
        fd: OwnedFd,
        _: Seat<Self>,
        data: &SelectionData,
    ) {
        match data {
            SelectionData::Text(text) => self.send_text(&mime, fd, text),
            SelectionData::X11 => self.serve_selection(target, mime, fd),
        }
    }
}

impl PrimarySelectionHandler for State {
    fn primary_selection_state(&mut self) -> &mut PrimarySelectionState {
        &mut self.clipboard.primary_state
    }
}

impl ext_data_control::DataControlHandler for State {
    fn data_control_state(&mut self) -> &mut ext_data_control::DataControlState {
        &mut self.clipboard.ext_data_control
    }
}

impl wlr_data_control::DataControlHandler for State {
    fn data_control_state(&mut self) -> &mut wlr_data_control::DataControlState {
        &mut self.clipboard.wlr_data_control
    }
}

impl State {
    pub(super) fn clipboard_active(&self) -> bool {
        self.clipboard.pending_offer.is_some()
            || self.clipboard.read.is_some()
            || self.clipboard.orphaned
            || !self.clipboard.conversions.is_empty()
            || !self.clipboard.writes.is_empty()
    }

    pub(super) fn clipboard_set(&mut self, text: &ClipboardText) {
        self.clipboard_own(Arc::from(text.as_str()));
        self.emit(Event::Clipboard(text.clone()));
    }

    /// Serves `text` as the clipboard from the compositor itself.
    fn clipboard_own(&mut self, text: Arc<str>) {
        self.clipboard.pending_offer = None;
        self.clipboard.read = None;
        self.clipboard.orphaned = false;
        *self.clipboard.owner(SelectionTarget::Clipboard) = Owner::default();
        self.clipboard.kept = Some(Arc::clone(&text));
        set_data_device_selection(
            &self.display_handle,
            &self.seat,
            strings(&WAYLAND_TEXT),
            SelectionData::Text(text),
        );
        if let Some(xwm) = &mut self.xwm
            && let Err(error) =
                xwm.new_selection(SelectionTarget::Clipboard, Some(strings(&X11_TEXT)))
        {
            tracing::warn!(%error, "forward clipboard ownership to X11");
        }
        self.x11_stacking = None;
    }

    /// An X11 client took `target`.
    pub(crate) fn x11_selection(&mut self, target: SelectionTarget, types: Vec<String>) {
        let offered = with_text(types.clone(), &WAYLAND_TEXT);
        match target {
            SelectionTarget::Clipboard => set_data_device_selection(
                &self.display_handle,
                &self.seat,
                offered,
                SelectionData::X11,
            ),
            SelectionTarget::Primary => set_primary_selection(
                &self.display_handle,
                &self.seat,
                offered,
                SelectionData::X11,
            ),
        }
        *self.clipboard.owner(target) = Owner {
            types: types.clone(),
            wayland: false,
        };
        if target == SelectionTarget::Clipboard {
            self.clipboard_offer(types);
        }
    }

    /// The X11 client owning `target` released it, usually by exiting.
    pub(crate) fn x11_selection_cleared(&mut self, target: SelectionTarget) {
        let x11 = match target {
            SelectionTarget::Clipboard => current_data_device_selection_userdata(&self.seat)
                .is_some_and(|data| matches!(*data, SelectionData::X11)),
            SelectionTarget::Primary => current_primary_selection_userdata(&self.seat)
                .is_some_and(|data| matches!(*data, SelectionData::X11)),
        };
        if x11 {
            self.selection_orphaned(target);
        }
    }

    /// The client owning `target` is gone. The clipboard keeps its text; the
    /// primary selection empties, as it does on X11.
    fn selection_orphaned(&mut self, target: SelectionTarget) {
        let owner = std::mem::take(self.clipboard.owner(target));
        match target {
            SelectionTarget::Clipboard => self.clipboard.orphaned = true,
            SelectionTarget::Primary if owner.wayland => {
                if let Some(xwm) = &mut self.xwm
                    && let Err(error) = xwm.new_selection(target, None)
                {
                    tracing::warn!(%error, "clear X11 primary selection");
                }
                self.x11_stacking = None;
            }
            SelectionTarget::Primary => clear_primary_selection(&self.display_handle, &self.seat),
        }
    }

    /// Whether the Wayland source of `target` still exists.
    fn wayland_selection_alive(&self, target: SelectionTarget) -> bool {
        // Probe with a type no source offers: only a vanished selection
        // reports `NoSelection`.
        let Ok(fd) = File::open("/dev/null") else {
            return true;
        };
        match target {
            SelectionTarget::Clipboard => !matches!(
                request_data_device_client_selection(&self.seat, String::new(), fd.into()),
                Err(data_device::SelectionRequestError::NoSelection)
            ),
            SelectionTarget::Primary => !matches!(
                request_primary_client_selection(&self.seat, String::new(), fd.into()),
                Err(primary_selection::SelectionRequestError::NoSelection)
            ),
        }
    }

    /// Writes `target` as `mime` into `fd` for a client that does not own it.
    pub(crate) fn serve_selection(&mut self, target: SelectionTarget, mime: String, fd: OwnedFd) {
        let text = match target {
            SelectionTarget::Clipboard => current_data_device_selection_userdata(&self.seat)
                .and_then(|data| match &*data {
                    SelectionData::Text(text) => Some(Arc::clone(text)),
                    SelectionData::X11 => None,
                }),
            SelectionTarget::Primary => None,
        };
        if let Some(text) = text {
            self.send_text(&mime, fd, &text);
            return;
        }
        let types = &self.clipboard.owner(target).types;
        let source = TEXT
            .into_iter()
            .find(|text| types.iter().any(|t| t == text));
        // Wayland sources such as wl-copy offer `STRING` but write UTF-8 for
        // it, so produce ISO 8859-1 from a UTF-8 type whenever there is one.
        let native =
            types.contains(&mime) && !(latin1(&mime) && source.is_some_and(|s| !latin1(s)));
        let result = if native {
            self.fetch(target, mime, fd)
        } else if let Some(source) = source
            && TEXT.contains(&mime.as_str())
        {
            self.convert(target, source, &mime, fd)
        } else {
            tracing::debug!(?target, mime, "selection owner does not offer this type");
            Ok(())
        };
        if let Err(error) = result {
            tracing::warn!(%error, ?target, "selection transfer failed");
        }
    }

    /// Asks the owner of `target` to write `mime` into `fd`.
    fn fetch(&mut self, target: SelectionTarget, mime: String, fd: OwnedFd) -> anyhow::Result<()> {
        if self.clipboard.owner(target).wayland {
            match target {
                SelectionTarget::Clipboard => {
                    request_data_device_client_selection(&self.seat, mime, fd)?;
                }
                SelectionTarget::Primary => request_primary_client_selection(&self.seat, mime, fd)?,
            }
        } else if let Some(xwm) = &mut self.xwm {
            xwm.send_selection(target, mime, fd)?;
        }
        Ok(())
    }

    /// Reads `source` from the owner of `target` and writes it into `fd` in
    /// the encoding of `mime`.
    fn convert(
        &mut self,
        target: SelectionTarget,
        source: &str,
        mime: &str,
        fd: OwnedFd,
    ) -> anyhow::Result<()> {
        if self.clipboard.conversions.len() >= MAX_TRANSFERS {
            tracing::warn!(
                ?target,
                mime,
                "too many selection conversions in flight; dropped one"
            );
            return Ok(());
        }
        let (read, write) = nonblocking_pipe()?;
        self.fetch(target, source.into(), write)?;
        self.clipboard.conversions.push((
            Read::new(read, latin1(source), "type conversion"),
            fd,
            latin1(mime),
        ));
        Ok(())
    }

    fn send_text(&mut self, mime: &str, fd: OwnedFd, text: &str) {
        if !TEXT.contains(&mime) {
            tracing::debug!(mime, "compositor selection holds only text");
            return;
        }
        self.queue_write(fd, encode(text, latin1(mime)));
    }

    fn queue_write(&mut self, fd: OwnedFd, bytes: Arc<[u8]>) {
        if self.clipboard.writes.len() >= MAX_TRANSFERS {
            tracing::warn!("too many selection writes in flight; dropped one");
            return;
        }
        if let Ok(flags) = fcntl(&fd, FcntlArg::F_GETFL)
            && fcntl(
                &fd,
                FcntlArg::F_SETFL(OFlag::from_bits_truncate(flags) | OFlag::O_NONBLOCK),
            )
            .is_ok()
        {
            self.clipboard.writes.push(Write {
                fd: File::from(fd),
                bytes,
                offset: 0,
                deadline: Instant::now() + TIMEOUT,
            });
        }
    }

    pub(crate) fn clipboard_offer(&mut self, types: Vec<String>) {
        self.clipboard.read = None;
        self.clipboard.kept = None;
        self.clipboard.orphaned = false;
        // Smithay invokes new_selection before installing the seat's selection.
        // Defer the request until dispatch completes, replacing older pending offers.
        self.clipboard.pending_offer = Some(types);
    }

    fn clipboard_begin_read(&mut self, types: &[String]) {
        let Some(mime) = TEXT
            .into_iter()
            .find(|mime| types.iter().any(|t| t == mime))
        else {
            return;
        };
        let result = (|| -> anyhow::Result<()> {
            let (read, write) = nonblocking_pipe()?;
            self.fetch(SelectionTarget::Clipboard, mime.into(), write)?;
            self.clipboard.read = Some(Read::new(read, latin1(mime), "viewer sync"));
            Ok(())
        })();
        if let Err(error) = result {
            tracing::warn!(%error, "clipboard receive failed");
        }
    }

    pub(super) fn clipboard_tick(&mut self) {
        if self.clipboard.client_left.swap(false, Ordering::Relaxed) {
            for target in [SelectionTarget::Clipboard, SelectionTarget::Primary] {
                if self.clipboard.owner(target).wayland && !self.wayland_selection_alive(target) {
                    self.selection_orphaned(target);
                }
            }
        }
        let mut completed = None;
        if let Some(types) = self.clipboard.pending_offer.take() {
            if types.is_empty() {
                completed = ClipboardText::new(String::new()).ok();
            } else {
                self.clipboard_begin_read(&types);
            }
        }
        if let Some(read) = self.clipboard.read.as_mut() {
            match read.poll() {
                Progress::Pending => {}
                Progress::Done(text) => {
                    self.clipboard.read = None;
                    match ClipboardText::new(text) {
                        Ok(text) => {
                            self.clipboard.kept = Some(Arc::from(text.as_str()));
                            completed = Some(text);
                        }
                        Err(error) => tracing::warn!(
                            %error,
                            "clipboard text exceeds the viewer limit; not synced or kept"
                        ),
                    }
                }
                Progress::Failed => self.clipboard.read = None,
            }
        }
        // A fresh selection completes a copy even when its text is unchanged.
        if let Some(text) = completed {
            self.emit(Event::Clipboard(text));
        }
        if self.clipboard.orphaned
            && self.clipboard.pending_offer.is_none()
            && self.clipboard.read.is_none()
        {
            self.clipboard_keep();
        }
        for (mut read, fd, latin1) in std::mem::take(&mut self.clipboard.conversions) {
            match read.poll() {
                Progress::Pending => self.clipboard.conversions.push((read, fd, latin1)),
                Progress::Done(text) => self.queue_write(fd, encode(&text, latin1)),
                Progress::Failed => {}
            }
        }
        self.clipboard.writes.retain_mut(|write| {
            if Instant::now() >= write.deadline {
                tracing::warn!(
                    written = write.offset,
                    bytes = write.bytes.len(),
                    "selection reader did not finish within {TIMEOUT:?}; dropped the transfer"
                );
                return false;
            }
            let end = write.bytes.len().min(write.offset + 65536);
            match write.fd.write(&write.bytes[write.offset..end]) {
                Ok(0) => false,
                Ok(n) => {
                    write.offset += n;
                    write.offset < write.bytes.len()
                }
                Err(error) => matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                ),
            }
        });
    }

    /// Serves the exited owner's text, or empties the clipboard when none was kept.
    fn clipboard_keep(&mut self) {
        if let Some(text) = self.clipboard.kept.clone() {
            tracing::debug!(
                bytes = text.len(),
                "kept clipboard text after its owner exited"
            );
            self.clipboard_own(text);
        } else {
            self.clipboard.orphaned = false;
            clear_data_device_selection(&self.display_handle, &self.seat);
            if let Some(xwm) = &mut self.xwm
                && let Err(error) = xwm.new_selection(SelectionTarget::Clipboard, None)
            {
                tracing::warn!(%error, "clear X11 clipboard");
            }
            self.x11_stacking = None;
            if let Ok(text) = ClipboardText::new(String::new()) {
                self.emit(Event::Clipboard(text));
            }
        }
        // Ticks run outside dispatch; flush the XWM's ownership change now.
        crate::xwayland::sync_stacking(self);
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    fn offer_read(state: &mut State, text: &str) {
        offer_bytes(state, text.as_bytes(), false);
    }

    /// Stands in for the owner's reply to the viewer read; tests have no XWM.
    fn offer_bytes(state: &mut State, bytes: &[u8], latin1: bool) {
        let (read, write) =
            nix::unistd::pipe2(OFlag::O_CLOEXEC | OFlag::O_NONBLOCK).expect("clipboard test pipe");
        File::from(write)
            .write_all(bytes)
            .expect("write clipboard offer");
        state.clipboard.read = Some(Read::new(read.into(), latin1, "test"));
    }

    /// Requests `mime` from the clipboard as a client does and returns the reply.
    fn request(state: &mut State, mime: &str) -> Vec<u8> {
        let (read, write) = nix::unistd::pipe2(OFlag::O_CLOEXEC).expect("request pipe");
        state.serve_selection(SelectionTarget::Clipboard, mime.into(), write);
        while !state.clipboard.writes.is_empty() {
            state.clipboard_tick();
        }
        let mut bytes = Vec::new();
        File::from(read)
            .read_to_end(&mut bytes)
            .expect("read reply");
        bytes
    }

    fn clipboard(text: &str) -> Event {
        Event::Clipboard(ClipboardText::new(text.into()).expect("valid clipboard text"))
    }

    #[test]
    fn string_requests_get_iso_8859_1() {
        let (_display, mut state) = crate::tests::test_state();
        state.clipboard_set(&ClipboardText::new("café €".into()).expect("valid clipboard text"));
        assert_eq!(request(&mut state, "STRING"), b"caf\xe9 ?");
        assert_eq!(request(&mut state, MIME), "café €".as_bytes());
        assert_eq!(request(&mut state, "UTF8_STRING"), "café €".as_bytes());
        assert_eq!(request(&mut state, "image/png"), b"");
    }

    #[test]
    fn clipboard_text_outlives_an_owner_that_exits_mid_read() {
        let (_display, mut state) = crate::tests::test_state();
        state.x11_selection(SelectionTarget::Clipboard, vec!["STRING".into()]);
        state.clipboard.pending_offer = None;
        offer_bytes(&mut state, b"na\xefve", true);
        state.clipboard_tick(); // Buffered; the end of the reply is still to come.
        state.x11_selection_cleared(SelectionTarget::Clipboard);
        state.clipboard_tick();
        state.clipboard_tick();
        assert_eq!(state.event_sink.take_events(), vec![clipboard("naïve")]);
        assert!(matches!(
            current_data_device_selection_userdata(&state.seat).as_deref(),
            Some(SelectionData::Text(text)) if &**text == "naïve"
        ));
        assert_eq!(request(&mut state, MIME), "naïve".as_bytes());
        assert_eq!(request(&mut state, "STRING"), b"na\xefve");

        // Only text is kept: an exiting image owner leaves an empty clipboard.
        state.x11_selection(SelectionTarget::Clipboard, vec!["image/png".into()]);
        state.clipboard_tick();
        state.x11_selection_cleared(SelectionTarget::Clipboard);
        state.clipboard_tick();
        assert_eq!(state.event_sink.take_events(), vec![clipboard("")]);
        assert!(current_data_device_selection_userdata(&state.seat).is_none());
    }

    #[derive(Clone, Default)]
    struct Logs(Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for Logs {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .map_err(|error| std::io::Error::other(error.to_string()))?
                .extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn oversized_clipboard_is_logged_and_releases_its_owner() {
        let logs = Logs::default();
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(false)
            .with_writer({
                let logs = logs.clone();
                move || logs.clone()
            })
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            let (_display, mut state) = crate::tests::test_state();
            let (read, write) = nonblocking_pipe().expect("clipboard test pipe");
            state.clipboard.read = Some(Read::new(read, false, "test"));
            let owner = std::thread::spawn(move || {
                File::from(write).write_all(&vec![b'x'; 2 * MAX_CLIPBOARD_BYTES])
            });
            // Well inside TIMEOUT, so the size limit is what ends the read.
            let deadline = Instant::now() + Duration::from_secs(2);
            while state.clipboard.read.is_some() {
                assert!(Instant::now() < deadline, "oversized read did not end");
                state.clipboard_tick();
                std::thread::sleep(Duration::from_millis(1));
            }
            let error = owner
                .join()
                .expect("owner thread")
                .expect_err("the owner's write must fail once the read is dropped");
            assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe);
            assert!(state.event_sink.take_events().is_empty());
        });
        let logs = String::from_utf8(logs.0.lock().expect("log buffer").clone()).expect("logs");
        assert!(
            logs.contains("WARN")
                && logs.contains("exceeds the transfer limit")
                && logs.contains("limit=1048576"),
            "{logs}"
        );
    }

    #[test]
    fn each_completed_selection_notifies_even_when_text_is_identical() {
        let (_display, mut state) = crate::tests::test_state();
        let text = ClipboardText::new("copy β 73".into()).expect("valid clipboard text");
        for _ in 0..2 {
            offer_read(&mut state, text.as_str());
            state.clipboard_tick();
            state.clipboard_tick();
            assert_eq!(
                state.event_sink.take_events(),
                vec![Event::Clipboard(text.clone())]
            );
            state.clipboard_tick();
            assert!(
                state.event_sink.take_events().is_empty(),
                "idle is not a new selection"
            );
        }
        for _ in 0..2 {
            state.clipboard_offer(Vec::new());
            state.clipboard_tick();
            assert_eq!(
                state.event_sink.take_events(),
                vec![Event::Clipboard(
                    ClipboardText::new(String::new()).expect("empty clipboard")
                )]
            );
        }
    }

    /// A data-control client: a clipboard manager or wl-clipboard.
    mod data_control {
        use std::collections::HashMap;
        use std::io::Write as _;

        use wayland_client::Connection;
        use wayland_client::Dispatch;
        use wayland_client::Proxy;
        use wayland_client::QueueHandle;
        use wayland_client::backend::ObjectId;
        use wayland_client::protocol::wl_registry;
        use wayland_client::protocol::wl_seat::WlSeat;
        use wayland_protocols::ext::data_control::v1::client as ext;
        use wayland_protocols_wlr::data_control::v1::client as wlr;

        pub(super) enum Offer {
            Ext(ext::ext_data_control_offer_v1::ExtDataControlOfferV1),
            Wlr(wlr::zwlr_data_control_offer_v1::ZwlrDataControlOfferV1),
        }

        pub(super) enum Device {
            Ext(ext::ext_data_control_device_v1::ExtDataControlDeviceV1),
            Wlr(wlr::zwlr_data_control_device_v1::ZwlrDataControlDeviceV1),
        }

        #[derive(Default)]
        pub(super) struct App {
            pub(super) ext: bool,
            pub(super) seat: Option<WlSeat>,
            pub(super) ext_manager:
                Option<ext::ext_data_control_manager_v1::ExtDataControlManagerV1>,
            pub(super) wlr_manager:
                Option<wlr::zwlr_data_control_manager_v1::ZwlrDataControlManagerV1>,
            pub(super) device: Option<Device>,
            pub(super) mimes: HashMap<ObjectId, Vec<String>>,
            /// The clipboard and primary offers, `None` once a selection is unset.
            pub(super) selections: [Option<(Offer, Vec<String>)>; 2],
            /// Text each source this client created serves.
            pub(super) served: HashMap<ObjectId, String>,
        }

        impl App {
            fn selected(&mut self, primary: bool, offer: Option<Offer>) {
                let selection = offer.map(|offer| {
                    let id = match &offer {
                        Offer::Ext(offer) => offer.id(),
                        Offer::Wlr(offer) => offer.id(),
                    };
                    let mimes = self.mimes.remove(&id).unwrap_or_default();
                    (offer, mimes)
                });
                self.selections[usize::from(primary)] = selection;
            }

            fn serve(&self, source: &ObjectId, fd: std::os::fd::OwnedFd) {
                let text = self.served.get(source).cloned().unwrap_or_default();
                std::fs::File::from(fd)
                    .write_all(text.as_bytes())
                    .expect("serve the selection");
            }
        }

        impl Dispatch<wl_registry::WlRegistry, ()> for App {
            fn event(
                app: &mut Self,
                registry: &wl_registry::WlRegistry,
                event: wl_registry::Event,
                (): &(),
                _: &Connection,
                qh: &QueueHandle<Self>,
            ) {
                let wl_registry::Event::Global {
                    name, interface, ..
                } = event
                else {
                    return;
                };
                if interface == WlSeat::interface().name {
                    app.seat = Some(registry.bind(name, 1, qh, ()));
                } else if app.ext
                    && interface
                        == ext::ext_data_control_manager_v1::ExtDataControlManagerV1::interface()
                            .name
                {
                    app.ext_manager = Some(registry.bind(name, 1, qh, ()));
                } else if !app.ext
                    && interface
                        == wlr::zwlr_data_control_manager_v1::ZwlrDataControlManagerV1::interface()
                            .name
                {
                    app.wlr_manager = Some(registry.bind(name, 2, qh, ()));
                }
            }
        }

        wayland_client::delegate_noop!(App: ignore WlSeat);
        wayland_client::delegate_noop!(App: ext::ext_data_control_manager_v1::ExtDataControlManagerV1);
        wayland_client::delegate_noop!(App: wlr::zwlr_data_control_manager_v1::ZwlrDataControlManagerV1);

        macro_rules! data_control_client {
            ($variant:ident, $device:ident, $offer:ident, $source:ident) => {
                impl Dispatch<$device::$device, ()> for App {
                    fn event(
                        app: &mut Self,
                        _: &$device::$device,
                        event: $device::Event,
                        (): &(),
                        _: &Connection,
                        _: &QueueHandle<Self>,
                    ) {
                        match event {
                            $device::Event::DataOffer { id } => {
                                app.mimes.insert(id.id(), Vec::new());
                            }
                            $device::Event::Selection { id } => {
                                app.selected(false, id.map(Offer::$variant));
                            }
                            $device::Event::PrimarySelection { id } => {
                                app.selected(true, id.map(Offer::$variant));
                            }
                            _ => {}
                        }
                    }

                    wayland_client::event_created_child!(App, $device::$device, [
                        $device::EVT_DATA_OFFER_OPCODE => ($offer::$offer, ()),
                    ]);
                }

                impl Dispatch<$offer::$offer, ()> for App {
                    fn event(
                        app: &mut Self,
                        offer: &$offer::$offer,
                        event: $offer::Event,
                        (): &(),
                        _: &Connection,
                        _: &QueueHandle<Self>,
                    ) {
                        if let $offer::Event::Offer { mime_type } = event {
                            app.mimes.entry(offer.id()).or_default().push(mime_type);
                        }
                    }
                }

                impl Dispatch<$source::$source, ()> for App {
                    fn event(
                        app: &mut Self,
                        source: &$source::$source,
                        event: $source::Event,
                        (): &(),
                        _: &Connection,
                        _: &QueueHandle<Self>,
                    ) {
                        if let $source::Event::Send { fd, .. } = event {
                            app.serve(&source.id(), fd);
                        }
                    }
                }
            };
        }

        use ext::ext_data_control_device_v1 as ExtDataControlDeviceV1;
        use ext::ext_data_control_offer_v1 as ExtDataControlOfferV1;
        use ext::ext_data_control_source_v1 as ExtDataControlSourceV1;
        use wlr::zwlr_data_control_device_v1 as ZwlrDataControlDeviceV1;
        use wlr::zwlr_data_control_offer_v1 as ZwlrDataControlOfferV1;
        use wlr::zwlr_data_control_source_v1 as ZwlrDataControlSourceV1;
        data_control_client!(
            Ext,
            ExtDataControlDeviceV1,
            ExtDataControlOfferV1,
            ExtDataControlSourceV1
        );
        data_control_client!(
            Wlr,
            ZwlrDataControlDeviceV1,
            ZwlrDataControlOfferV1,
            ZwlrDataControlSourceV1
        );
    }

    use std::os::fd::AsFd as _;

    use wayland_client::Proxy as _;

    struct Client {
        connection: wayland_client::Connection,
        queue: wayland_client::EventQueue<data_control::App>,
        app: data_control::App,
    }

    impl Client {
        fn connect(
            display: &mut smithay::reexports::wayland_server::Display<State>,
            state: &mut State,
            ext: bool,
        ) -> Self {
            let (client, server) = std::os::unix::net::UnixStream::pair().expect("socket pair");
            display
                .handle()
                .insert_client(server, Arc::new(crate::shell::ClientState::new(state)))
                .expect("insert client");
            let connection =
                wayland_client::Connection::from_socket(client).expect("client connection");
            let queue = connection.new_event_queue();
            connection.display().get_registry(&queue.handle(), ());
            let mut client = Self {
                connection,
                queue,
                app: data_control::App {
                    ext,
                    ..Default::default()
                },
            };
            pump(display, state, &mut [&mut client]);
            pump(display, state, &mut [&mut client]);
            let qh = client.queue.handle();
            let seat = client.app.seat.as_ref().expect("wl_seat");
            let device = if ext {
                data_control::Device::Ext(
                    client
                        .app
                        .ext_manager
                        .as_ref()
                        .expect("ext-data-control global")
                        .get_data_device(seat, &qh, ()),
                )
            } else {
                data_control::Device::Wlr(
                    client
                        .app
                        .wlr_manager
                        .as_ref()
                        .expect("wlr-data-control global")
                        .get_data_device(seat, &qh, ()),
                )
            };
            client.app.device = Some(device);
            pump(display, state, &mut [&mut client]);
            client
        }

        /// Owns the clipboard or the primary selection with `text`.
        fn copy(&mut self, primary: bool, text: &str) {
            let qh = self.queue.handle();
            match self.app.device.as_ref().expect("device") {
                data_control::Device::Ext(device) => {
                    let manager = self.app.ext_manager.as_ref().expect("manager");
                    let source = manager.create_data_source(&qh, ());
                    source.offer(MIME.into());
                    self.app.served.insert(source.id(), text.into());
                    if primary {
                        device.set_primary_selection(Some(&source));
                    } else {
                        device.set_selection(Some(&source));
                    }
                }
                data_control::Device::Wlr(device) => {
                    let manager = self.app.wlr_manager.as_ref().expect("manager");
                    let source = manager.create_data_source(&qh, ());
                    source.offer(MIME.into());
                    self.app.served.insert(source.id(), text.into());
                    if primary {
                        device.set_primary_selection(Some(&source));
                    } else {
                        device.set_selection(Some(&source));
                    }
                }
            }
        }
    }

    /// Lets the server and every client exchange what they have queued.
    fn pump(
        display: &mut smithay::reexports::wayland_server::Display<State>,
        state: &mut State,
        clients: &mut [&mut Client],
    ) {
        for client in clients.iter_mut() {
            client.connection.flush().expect("client flush");
        }
        display.dispatch_clients(state).expect("server dispatch");
        display.flush_clients().expect("server flush");
        state.clipboard_tick();
        for client in clients.iter_mut() {
            if let Some(guard) = client.connection.prepare_read() {
                let mut poll = [nix::poll::PollFd::new(
                    guard.connection_fd(),
                    nix::poll::PollFlags::POLLIN,
                )];
                if nix::poll::poll(&mut poll, nix::poll::PollTimeout::ZERO).unwrap_or(0) > 0 {
                    guard.read().expect("client read");
                }
            }
            client
                .queue
                .dispatch_pending(&mut client.app)
                .expect("client dispatch");
            client.connection.flush().expect("client flush");
        }
    }

    /// Pastes `primary` or the clipboard as `client`, while `others` keep
    /// running; `None` when the selection is unset.
    fn paste(
        display: &mut smithay::reexports::wayland_server::Display<State>,
        state: &mut State,
        client: &mut Client,
        others: &mut [&mut Client],
        primary: bool,
    ) -> Option<String> {
        for _ in 0..4 {
            pump(display, state, &mut [&mut *client]);
            pump(display, state, others);
        }
        let (offer, mimes) = client.app.selections[usize::from(primary)].as_ref()?;
        assert!(mimes.iter().any(|mime| mime == MIME), "offered {mimes:?}");
        let (read, write) = nonblocking_pipe().expect("paste pipe");
        match offer {
            data_control::Offer::Ext(offer) => offer.receive(MIME.into(), write.as_fd()),
            data_control::Offer::Wlr(offer) => offer.receive(MIME.into(), write.as_fd()),
        }
        drop(write);
        let mut read = Read::new(read, false, "test paste");
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            assert!(Instant::now() < deadline, "paste did not finish");
            pump(display, state, &mut [&mut *client]);
            pump(display, state, others);
            match read.poll() {
                Progress::Pending => std::thread::sleep(Duration::from_millis(1)),
                Progress::Done(text) => return Some(text),
                Progress::Failed => panic!("paste failed"),
            }
        }
    }

    #[test]
    fn data_control_clients_share_the_selections_without_focus() {
        let (mut display, mut state) = crate::tests::test_state();
        let mut ext = Client::connect(&mut display, &mut state, true);
        let mut wlr = Client::connect(&mut display, &mut state, false);
        let keyboard = state.seat.get_keyboard().expect("seat keyboard");
        assert!(keyboard.current_focus().is_none());
        state.event_sink.take_events();

        ext.copy(false, "ext α");
        ext.copy(true, "ext primary β");
        let pasted = paste(&mut display, &mut state, &mut wlr, &mut [&mut ext], false);
        assert_eq!(pasted.as_deref(), Some("ext α"));
        let pasted = paste(&mut display, &mut state, &mut wlr, &mut [&mut ext], true);
        assert_eq!(pasted.as_deref(), Some("ext primary β"));
        // The viewer syncs the clipboard, never the primary selection.
        assert_eq!(state.event_sink.take_events(), vec![clipboard("ext α")]);

        wlr.copy(false, "wlr γ");
        let pasted = paste(&mut display, &mut state, &mut ext, &mut [&mut wlr], false);
        assert_eq!(pasted.as_deref(), Some("wlr γ"));
        assert_eq!(state.event_sink.take_events(), vec![clipboard("wlr γ")]);
        assert!(keyboard.current_focus().is_none());

        // Owners exit: the clipboard keeps its text, the primary selection ends.
        drop(ext);
        drop(wlr);
        for _ in 0..4 {
            pump(&mut display, &mut state, &mut []);
        }
        let mut reader = Client::connect(&mut display, &mut state, true);
        let pasted = paste(&mut display, &mut state, &mut reader, &mut [], false);
        assert_eq!(pasted.as_deref(), Some("wlr γ"));
        assert_eq!(
            paste(&mut display, &mut state, &mut reader, &mut [], true),
            None
        );
        assert!(state.event_sink.take_events().is_empty());
    }

    #[test]
    fn browser_write_publishes_the_new_snapshot_and_cancels_obsolete_reads() {
        let (_display, mut state) = crate::tests::test_state();
        offer_read(&mut state, "old native text");
        state.clipboard_tick(); // Buffered, but not yet published at EOF.
        assert!(state.event_sink.take_events().is_empty());
        let text = ClipboardText::new("new browser text".into()).expect("valid clipboard text");
        state.clipboard_set(&text);
        state.clipboard_tick();
        assert!(state.clipboard.read.is_none());
        assert_eq!(
            state.event_sink.take_events(),
            vec![Event::Clipboard(text.clone())]
        );

        // A later native copy of the browser's text is still a fresh copy event.
        offer_read(&mut state, text.as_str());
        state.clipboard_tick();
        state.clipboard_tick();
        assert_eq!(
            state.event_sink.take_events(),
            vec![Event::Clipboard(text.clone())]
        );

        state.clipboard_offer(Vec::new()); // A queued clear must not replace a newer write.
        state.clipboard_set(&text);
        state.clipboard_tick();
        assert_eq!(state.event_sink.take_events(), vec![Event::Clipboard(text)]);
    }
}
