use std::{
    fs::File,
    io::{Read, Write},
    os::fd::{AsFd, OwnedFd},
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
    },
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, bail};
use calloop::channel::SyncSender;
use nix::{
    fcntl::{FcntlArg, OFlag, fcntl},
    unistd::pipe,
};
use wayland_client::{QueueHandle, protocol::wl_seat};
use wayland_protocols::ext::data_control::v1::client::{
    ext_data_control_device_v1, ext_data_control_manager_v1, ext_data_control_offer_v1,
    ext_data_control_source_v1,
};

use super::{ControlMessage, State};
use crate::protocol::MAX_CLIPBOARD_BYTES;

const TEXT_MIMES: [&str; 3] = ["text/plain;charset=utf-8", "UTF8_STRING", "text/plain"];
const MAX_INCOMING_TRANSFERS: usize = 4;
const MAX_OUTGOING_TRANSFERS: usize = 4;
const TRANSFER_STALL_LIMIT: Duration = Duration::from_secs(2);

struct Offer {
    proxy: ext_data_control_offer_v1::ExtDataControlOfferV1,
    mimes: Vec<String>,
}

struct Source {
    proxy: ext_data_control_source_v1::ExtDataControlSourceV1,
    payload: Arc<[u8]>,
}

pub struct Clipboard {
    pub manager: Option<ext_data_control_manager_v1::ExtDataControlManagerV1>,
    pub device: Option<ext_data_control_device_v1::ExtDataControlDeviceV1>,
    pub pending_offer: Option<ext_data_control_offer_v1::ExtDataControlOfferV1>,
    selection: Option<Offer>,
    source: Option<Source>,
    incoming_generation: Arc<AtomicU64>,
    shutting_down: Arc<AtomicBool>,
    incoming: Arc<AtomicUsize>,
    outgoing: Arc<AtomicUsize>,
    command_sender: SyncSender<ControlMessage>,
}

impl Clipboard {
    pub fn new(command_sender: SyncSender<ControlMessage>) -> Self {
        Self {
            manager: None,
            device: None,
            pending_offer: None,
            selection: None,
            source: None,
            incoming_generation: Arc::new(AtomicU64::new(0)),
            shutting_down: Arc::new(AtomicBool::new(false)),
            incoming: Arc::new(AtomicUsize::new(0)),
            outgoing: Arc::new(AtomicUsize::new(0)),
            command_sender,
        }
    }

    pub fn start(&mut self, seat: &wl_seat::WlSeat, qh: &QueueHandle<State>) -> Result<()> {
        let manager = self
            .manager
            .as_ref()
            .context("missing ext_data_control_manager_v1")?;
        self.device = Some(manager.get_data_device(seat, qh, ()));
        Ok(())
    }

    pub fn set_text(&mut self, text: String, qh: &QueueHandle<State>) -> Result<()> {
        if text.len() > MAX_CLIPBOARD_BYTES {
            bail!("clipboard exceeds one MiB");
        }
        let manager = self
            .manager
            .as_ref()
            .context("clipboard manager unavailable")?;
        let device = self
            .device
            .as_ref()
            .context("clipboard device unavailable")?;
        let proxy = manager.create_data_source(qh, ());
        for mime in TEXT_MIMES {
            proxy.offer(mime.to_owned());
        }
        device.set_selection(Some(&proxy));
        let source = Source {
            proxy,
            payload: Arc::from(text.into_bytes()),
        };
        if let Some(old) = self.source.replace(source) {
            old.proxy.destroy();
        }
        Ok(())
    }

    pub fn offer_mime(
        &mut self,
        proxy: &ext_data_control_offer_v1::ExtDataControlOfferV1,
        mime: String,
    ) {
        if self
            .selection
            .as_ref()
            .is_some_and(|offer| offer.proxy == *proxy)
        {
            self.selection.as_mut().unwrap().mimes.push(mime);
        } else if self.pending_offer.as_ref() == Some(proxy) {
            self.selection = Some(Offer {
                proxy: proxy.clone(),
                mimes: vec![mime],
            });
        }
    }

    pub fn select(
        &mut self,
        selected: Option<ext_data_control_offer_v1::ExtDataControlOfferV1>,
    ) -> Result<()> {
        let generation = self.incoming_generation.fetch_add(1, Ordering::SeqCst) + 1;
        let Some(proxy) = selected else {
            self.selection = None;
            self.command_sender
                .try_send(ControlMessage::ClipboardReceived {
                    generation,
                    result: Ok(String::new()),
                })
                .map_err(|_| anyhow::anyhow!("command queue full while clearing clipboard"))?;
            return Ok(());
        };
        if self
            .selection
            .as_ref()
            .is_none_or(|offer| offer.proxy != proxy)
        {
            self.selection = Some(Offer {
                proxy: proxy.clone(),
                mimes: Vec::new(),
            });
        }
        self.pending_offer = None;
        let offer = self.selection.as_ref().unwrap();
        let mime = TEXT_MIMES
            .iter()
            .find(|wanted| offer.mimes.iter().any(|offered| offered == **wanted))
            .context("selection offers no UTF-8 text MIME")?
            .to_string();
        let permit = TransferPermit::reserve(Arc::clone(&self.incoming), MAX_INCOMING_TRANSFERS)
            .context("too many clipboard reads are active")?;
        let (reader, writer) = pipe()?;
        set_nonblocking(&reader)?;
        proxy.receive(mime, writer.as_fd());
        drop(writer);
        spawn_read(
            reader,
            generation,
            Arc::clone(&self.incoming_generation),
            self.command_sender.clone(),
            permit,
        )?;
        Ok(())
    }

    pub fn send_requested(
        &self,
        requested_source: &ext_data_control_source_v1::ExtDataControlSourceV1,
        fd: OwnedFd,
    ) -> Result<()> {
        let Some(source) = self
            .source
            .as_ref()
            .filter(|source| source.proxy == *requested_source)
        else {
            return Ok(());
        };
        let Some(permit) =
            TransferPermit::reserve(Arc::clone(&self.outgoing), MAX_OUTGOING_TRANSFERS)
        else {
            return Ok(());
        };

        // An accepted send owns this immutable payload even if the selection changes later.
        let payload = Arc::clone(&source.payload);
        set_nonblocking(&fd)?;
        spawn_write(fd, payload, Arc::clone(&self.shutting_down), permit)?;
        Ok(())
    }

    pub fn cancel_source(&mut self, source: &ext_data_control_source_v1::ExtDataControlSourceV1) {
        if self
            .source
            .as_ref()
            .is_some_and(|active| active.proxy == *source)
        {
            self.source = None;
        }
        source.destroy();
    }

    pub fn accepts_transfer(&self, generation: u64) -> bool {
        self.incoming_generation.load(Ordering::SeqCst) == generation
    }

    pub fn cancel_transfers(&self) {
        self.incoming_generation.fetch_add(1, Ordering::SeqCst);
        self.shutting_down.store(true, Ordering::SeqCst);
    }
}

struct TransferPermit {
    active: Arc<AtomicUsize>,
}

impl TransferPermit {
    fn reserve(active: Arc<AtomicUsize>, limit: usize) -> Option<Self> {
        active
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |count| {
                (count < limit).then_some(count + 1)
            })
            .ok()?;
        Some(Self { active })
    }
}

impl Drop for TransferPermit {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::SeqCst);
    }
}

fn set_nonblocking(fd: &OwnedFd) -> Result<()> {
    let flags = OFlag::from_bits_truncate(fcntl(fd, FcntlArg::F_GETFL)?);
    fcntl(fd, FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK))?;
    Ok(())
}

fn spawn_write(
    fd: OwnedFd,
    payload: Arc<[u8]>,
    shutting_down: Arc<AtomicBool>,
    permit: TransferPermit,
) -> Result<thread::JoinHandle<()>> {
    thread::Builder::new()
        .name("streamd-clipboard-write".into())
        .spawn(move || {
            let _permit = permit;
            let mut file = File::from(fd);
            let started = Instant::now();
            let mut offset = 0;
            while offset < payload.len()
                && !shutting_down.load(Ordering::SeqCst)
                && started.elapsed() < TRANSFER_STALL_LIMIT
            {
                match file.write(&payload[offset..]) {
                    Ok(0) => break,
                    Ok(count) => offset += count,
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => break,
                }
            }
        })
        .context("start clipboard writer")
}

fn spawn_read(
    fd: OwnedFd,
    generation: u64,
    current_generation: Arc<AtomicU64>,
    sender: SyncSender<ControlMessage>,
    permit: TransferPermit,
) -> Result<thread::JoinHandle<()>> {
    thread::Builder::new()
        .name("streamd-clipboard-read".into())
        .spawn(move || {
            let _permit = permit;
            let mut file = File::from(fd);
            let mut bytes = Vec::new();
            let result = loop {
                if current_generation.load(Ordering::SeqCst) != generation {
                    return;
                }
                let mut part = [0_u8; 4096];
                match file.read(&mut part) {
                    Ok(0) => {
                        break String::from_utf8(bytes)
                            .map_err(|_| "clipboard is not UTF-8".to_string());
                    }
                    Ok(count) => {
                        if bytes.len() + count > MAX_CLIPBOARD_BYTES {
                            break Err("clipboard exceeds one MiB".to_string());
                        }
                        bytes.extend_from_slice(&part[..count]);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => break Err(error.to_string()),
                }
            };
            let _ = sender.try_send(ControlMessage::ClipboardReceived { generation, result });
        })
        .context("start clipboard reader")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_replacement_does_not_truncate_an_accepted_blocked_transfer() {
        let (reader, writer) = pipe().unwrap();
        set_nonblocking(&writer).unwrap();
        let active = Arc::new(AtomicUsize::new(0));
        let permit = TransferPermit::reserve(Arc::clone(&active), 1).unwrap();
        let shutting_down = Arc::new(AtomicBool::new(false));
        let payload_a: Arc<[u8]> = Arc::from(vec![b'a'; MAX_CLIPBOARD_BYTES]);
        let mut selected_payload = Arc::clone(&payload_a);
        let handle = spawn_write(
            writer,
            Arc::clone(&selected_payload),
            Arc::clone(&shutting_down),
            permit,
        )
        .unwrap();

        // Let the pipe fill before replacing the selected source.
        thread::sleep(Duration::from_millis(25));
        selected_payload = Arc::from(&b"source-b"[..]);
        let mut received = Vec::new();
        File::from(reader).read_to_end(&mut received).unwrap();
        handle.join().unwrap();

        assert_eq!(received, &*payload_a);
        assert_eq!(&*selected_payload, b"source-b");
        assert_eq!(active.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn shutdown_cancels_a_blocked_write_and_releases_its_thread_slot() {
        let (_reader, writer) = pipe().unwrap();
        set_nonblocking(&writer).unwrap();
        let active = Arc::new(AtomicUsize::new(0));
        let permit = TransferPermit::reserve(Arc::clone(&active), 1).unwrap();
        let shutting_down = Arc::new(AtomicBool::new(false));
        let handle = spawn_write(
            writer,
            Arc::from(vec![0_u8; MAX_CLIPBOARD_BYTES]),
            Arc::clone(&shutting_down),
            permit,
        )
        .unwrap();
        shutting_down.store(true, Ordering::SeqCst);
        handle.join().unwrap();

        assert_eq!(active.load(Ordering::SeqCst), 0);
        assert!(TransferPermit::reserve(active, 1).is_some());
    }
}
