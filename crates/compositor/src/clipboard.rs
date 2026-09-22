//! Nonblocking, bounded clipboard transfers. Replacing an offer drops its read FD,
//! so an obsolete producer can never overwrite a newer browser/client selection.
use std::fs::File;
use std::io::Read;
use std::io::Write;
use std::os::fd::OwnedFd;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

use nix::fcntl::FcntlArg;
use nix::fcntl::OFlag;
use nix::fcntl::fcntl;
use smithay::wayland::selection::SelectionSource;
use smithay::wayland::selection::data_device::request_data_device_client_selection;
use smithay::wayland::selection::data_device::set_data_device_selection;
use waywire_protocol::pipe::ClipboardText;
use waywire_protocol::pipe::Event;
use waywire_protocol::pipe::MAX_CLIPBOARD_BYTES;

use super::State;

const MIME: &str = "text/plain;charset=utf-8";
const TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Clone)]
pub(crate) enum SelectionData {
    Text(Arc<str>),
    X11,
}

#[derive(Default)]
pub(super) struct Clipboard {
    pending_offer: Option<Vec<String>>,
    read: Option<(File, Vec<u8>, Instant)>,
    writes: Vec<(File, Arc<str>, usize, Instant)>,
    last_text: Option<ClipboardText>,
}

impl State {
    pub(super) fn clipboard_set(&mut self, text: &ClipboardText) {
        self.clipboard.pending_offer = None;
        self.clipboard.read = None;
        self.clipboard.last_text = Some(text.clone());
        set_data_device_selection(
            &self.display_handle,
            &self.seat,
            vec![MIME.into(), "text/plain".into()],
            SelectionData::Text(Arc::<str>::from(text.as_str())),
        );
        if let Some(xwm) = &mut self.xwm
            && let Err(error) = xwm.new_selection(
                smithay::wayland::selection::SelectionTarget::Clipboard,
                Some(vec![MIME.into(), "text/plain".into()]),
            )
        {
            tracing::warn!(%error, "forward clipboard ownership to X11");
        }
    }

    pub(crate) fn send_clipboard(&mut self, mime: String, fd: OwnedFd) {
        let own = smithay::wayland::selection::data_device::current_data_device_selection_userdata(
            &self.seat,
        )
        .map(|data| data.clone());
        match own {
            Some(SelectionData::Text(text)) => self.clipboard_send(&mime, fd, text),
            Some(SelectionData::X11) => {}
            None => {
                if let Err(error) = request_data_device_client_selection(&self.seat, mime, fd) {
                    tracing::warn!(%error, "send native clipboard to X11");
                }
            }
        }
    }

    pub(super) fn clipboard_changed(&mut self, source: Option<SelectionSource>) {
        self.clipboard_offer(source.map(|source| source.mime_types()).unwrap_or_default());
    }

    pub(crate) fn clipboard_offer(&mut self, types: Vec<String>) {
        self.clipboard.read = None;
        // Smithay invokes new_selection before installing the seat's selection.
        // Defer the request until dispatch completes, replacing older pending offers.
        self.clipboard.pending_offer = Some(types);
    }

    fn clipboard_begin_read(&mut self, types: &[String]) {
        let Some(mime) = [MIME, "text/plain", "UTF8_STRING"]
            .into_iter()
            .find(|mime| types.iter().any(|t| t == mime))
        else {
            return;
        };
        let result = (|| -> anyhow::Result<()> {
            let (read, write) = nix::unistd::pipe2(OFlag::O_CLOEXEC)?;
            let flags = OFlag::from_bits_truncate(fcntl(&read, FcntlArg::F_GETFL)?);
            fcntl(&read, FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK))?;
            let x11 =
                smithay::wayland::selection::data_device::current_data_device_selection_userdata(
                    &self.seat,
                )
                .is_some_and(|data| matches!(*data, SelectionData::X11));
            if x11 {
                if let Some(xwm) = &mut self.xwm {
                    xwm.send_selection(
                        smithay::wayland::selection::SelectionTarget::Clipboard,
                        mime.into(),
                        write,
                    )?;
                }
            } else {
                request_data_device_client_selection(&self.seat, mime.into(), write)?;
            }
            self.clipboard.read = Some((File::from(read), Vec::new(), Instant::now() + TIMEOUT));
            Ok(())
        })();
        if let Err(error) = result {
            tracing::warn!(%error, "clipboard receive failed");
        }
    }

    pub(super) fn clipboard_send(&mut self, mime: &str, fd: OwnedFd, text: Arc<str>) {
        if ![MIME, "text/plain"].contains(&mime) || self.clipboard.writes.len() >= 8 {
            return;
        }
        if let Ok(flags) = fcntl(&fd, FcntlArg::F_GETFL)
            && fcntl(
                &fd,
                FcntlArg::F_SETFL(OFlag::from_bits_truncate(flags) | OFlag::O_NONBLOCK),
            )
            .is_ok()
        {
            self.clipboard
                .writes
                .push((File::from(fd), text, 0, Instant::now() + TIMEOUT));
        }
    }

    pub(super) fn clipboard_tick(&mut self) {
        let mut completed = None;
        if let Some(types) = self.clipboard.pending_offer.take() {
            if types.is_empty() {
                completed = ClipboardText::new(String::new()).ok();
            } else {
                self.clipboard_begin_read(&types);
            }
        }
        let mut keep = false;
        if let Some((fd, bytes, deadline)) = self.clipboard.read.as_mut() {
            let mut part = vec![0; 65536];
            if Instant::now() < *deadline {
                match fd.read(&mut part) {
                    Ok(0) => {
                        completed = String::from_utf8(std::mem::take(bytes))
                            .ok()
                            .and_then(|text| ClipboardText::new(text).ok());
                    }
                    Ok(n) if bytes.len() + n <= MAX_CLIPBOARD_BYTES => {
                        bytes.extend_from_slice(&part[..n]);
                        keep = true;
                    }
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                        ) =>
                    {
                        keep = true;
                    }
                    Ok(_) | Err(_) => {}
                }
            }
        }
        if !keep {
            self.clipboard.read = None;
        }
        if let Some(text) = completed
            && self.clipboard.last_text.as_ref() != Some(&text)
        {
            self.clipboard.last_text = Some(text.clone());
            self.emit(Event::Clipboard(text));
        }
        self.clipboard
            .writes
            .retain_mut(|(fd, text, offset, deadline)| {
                if Instant::now() >= *deadline {
                    return false;
                }
                match fd.write(&text.as_bytes()[*offset..text.len().min(*offset + 65536)]) {
                    Ok(0) => false,
                    Ok(n) => {
                        *offset += n;
                        *offset < text.len()
                    }
                    Err(error) => matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                    ),
                }
            });
    }
}
