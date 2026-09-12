use std::fs::File;
use std::io::Read;
use std::io::Write;
use std::os::fd::AsFd;
use std::os::fd::OwnedFd;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::AtomicU64;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use std::time::Instant;

use anyhow::Context;
use anyhow::Result;
use calloop::channel::SyncSender;
use nix::fcntl::FcntlArg;
use nix::fcntl::OFlag;
use nix::fcntl::fcntl;
use nix::unistd::pipe;
use tracing::warn;
use wayland_client::QueueHandle;
use wayland_client::protocol::wl_seat;
use wayland_protocols::ext::data_control::v1::client::ext_data_control_device_v1;
use wayland_protocols::ext::data_control::v1::client::ext_data_control_manager_v1;
use wayland_protocols::ext::data_control::v1::client::ext_data_control_offer_v1;
use wayland_protocols::ext::data_control::v1::client::ext_data_control_source_v1;
use waywire_protocol::pipe::ClipboardText;
use waywire_protocol::pipe::MAX_CLIPBOARD_BYTES;

use super::ControlMessage;
use super::State;

const TEXT_MIMES: [&str; 3] = ["text/plain;charset=utf-8", "UTF8_STRING", "text/plain"];
const MAX_INCOMING_TRANSFERS: usize = 4;
const MAX_OUTGOING_TRANSFERS: usize = 4;
const TRANSFER_STALL_LIMIT: Duration = Duration::from_secs(2);
const COMPLETION_STALL_LIMIT: Duration = Duration::from_secs(2);
const COMPLETION_RETRY_INTERVAL: Duration = Duration::from_millis(5);

struct Offer {
    proxy: ext_data_control_offer_v1::ExtDataControlOfferV1,
    mimes: Vec<String>,
}

struct Source {
    proxy: ext_data_control_source_v1::ExtDataControlSourceV1,
    payload: Arc<[u8]>,
}

pub(crate) struct Clipboard {
    pub(crate) manager: Option<ext_data_control_manager_v1::ExtDataControlManagerV1>,
    pub(crate) device: Option<ext_data_control_device_v1::ExtDataControlDeviceV1>,
    pub(crate) pending_offer: Option<ext_data_control_offer_v1::ExtDataControlOfferV1>,
    selection: Option<Offer>,
    source: Option<Source>,
    incoming_generation: Arc<AtomicU64>,
    shutting_down: Arc<AtomicBool>,
    incoming: Arc<AtomicUsize>,
    outgoing: Arc<AtomicUsize>,
    command_sender: SyncSender<ControlMessage>,
}

impl Clipboard {
    pub(crate) fn new(command_sender: SyncSender<ControlMessage>) -> Self {
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

    pub(crate) fn start(&mut self, seat: &wl_seat::WlSeat, qh: &QueueHandle<State>) -> Result<()> {
        let manager = self
            .manager
            .as_ref()
            .context("missing ext_data_control_manager_v1")?;
        self.device = Some(manager.get_data_device(seat, qh, ()));
        Ok(())
    }

    pub(crate) fn set_text(&mut self, text: &ClipboardText, qh: &QueueHandle<State>) -> Result<()> {
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
            payload: Arc::from(text.as_str().as_bytes()),
        };
        if let Some(old) = self.source.replace(source) {
            old.proxy.destroy();
        }
        Ok(())
    }

    pub(crate) fn offer_mime(
        &mut self,
        proxy: &ext_data_control_offer_v1::ExtDataControlOfferV1,
        mime: String,
    ) {
        if let Some(offer) = self
            .selection
            .as_mut()
            .filter(|offer| offer.proxy == *proxy)
        {
            offer.mimes.push(mime);
        } else if self.pending_offer.as_ref() == Some(proxy) {
            self.selection = Some(Offer {
                proxy: proxy.clone(),
                mimes: vec![mime],
            });
        }
    }

    pub(crate) fn select(
        &mut self,
        selected: Option<ext_data_control_offer_v1::ExtDataControlOfferV1>,
    ) -> Result<()> {
        let generation = self.incoming_generation.fetch_add(1, Ordering::SeqCst) + 1;
        let Some(proxy) = selected else {
            self.selection = None;
            self.command_sender
                .try_send(ControlMessage::ClipboardReceived {
                    generation,
                    result: ClipboardText::new(String::new()).map_err(|error| error.to_string()),
                })
                .map_err(|_error| anyhow::anyhow!("command queue full while clearing clipboard"))?;
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
        let offer = self
            .selection
            .as_ref()
            .context("selected clipboard offer is unavailable")?;
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

    pub(crate) fn send_requested(
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

    pub(crate) fn cancel_source(
        &mut self,
        source: &ext_data_control_source_v1::ExtDataControlSourceV1,
    ) {
        if self
            .source
            .as_ref()
            .is_some_and(|active| active.proxy == *source)
        {
            self.source = None;
        }
        source.destroy();
    }

    pub(crate) fn accepts_transfer(&self, generation: u64) -> bool {
        self.incoming_generation.load(Ordering::SeqCst) == generation
    }

    pub(crate) fn cancel_transfers(&self) {
        let _ = self.incoming_generation.fetch_add(1, Ordering::SeqCst);
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
        let _ = self.active.fetch_sub(1, Ordering::SeqCst);
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
            let permit_guard = permit;
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
            drop(permit_guard);
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
            let permit_guard = permit;
            let mut file = File::from(fd);
            let mut bytes = Vec::new();
            let result = loop {
                if current_generation.load(Ordering::SeqCst) != generation {
                    break None;
                }
                let mut part = [0_u8; 4096];
                match file.read(&mut part) {
                    Ok(0) => {
                        break Some(
                            String::from_utf8(bytes)
                                .map_err(|_error| "clipboard is not UTF-8".to_string())
                                .and_then(|text| {
                                    ClipboardText::new(text).map_err(|error| error.to_string())
                                }),
                        );
                    }
                    Ok(count) => {
                        if bytes.len() + count > MAX_CLIPBOARD_BYTES {
                            break Some(Err("clipboard exceeds one MiB".to_string()));
                        }
                        bytes.extend_from_slice(&part[..count]);
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => break Some(Err(error.to_string())),
                }
            };
            if let Some(result) = result {
                send_read_completion(
                    &sender,
                    &current_generation,
                    generation,
                    result,
                    COMPLETION_STALL_LIMIT,
                );
            }
            drop(permit_guard);
        })
        .context("start clipboard reader")
}

fn send_read_completion(
    sender: &SyncSender<ControlMessage>,
    current_generation: &AtomicU64,
    generation: u64,
    result: std::result::Result<ClipboardText, String>,
    stall_limit: Duration,
) {
    let started = Instant::now();
    let mut message = ControlMessage::ClipboardReceived { generation, result };
    loop {
        if current_generation.load(Ordering::SeqCst) != generation {
            return;
        }
        match sender.try_send(message) {
            Ok(()) => return,
            Err(mpsc::TrySendError::Disconnected(_message)) => {
                if current_generation.load(Ordering::SeqCst) == generation {
                    warn!(generation, "clipboard completion receiver disconnected");
                }
                return;
            }
            Err(mpsc::TrySendError::Full(returned)) => {
                message = returned;
                if current_generation.load(Ordering::SeqCst) != generation {
                    return;
                }
                if started.elapsed() >= stall_limit {
                    warn!(generation, "clipboard completion queue remained full");
                    return;
                }
                thread::sleep(COMPLETION_RETRY_INTERVAL);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn source_replacement_does_not_truncate_an_accepted_blocked_transfer() {
        let (reader, writer) = pipe().expect("test pipe should open");
        set_nonblocking(&writer).expect("test pipe writer should become nonblocking");
        let active = Arc::new(AtomicUsize::new(0));
        let permit = TransferPermit::reserve(Arc::clone(&active), 1)
            .expect("first outgoing transfer slot should be available");
        let shutting_down = Arc::new(AtomicBool::new(false));
        let payload_a: Arc<[u8]> = Arc::from(vec![b'a'; MAX_CLIPBOARD_BYTES]);
        let mut selected_payload = Arc::clone(&payload_a);
        let handle = spawn_write(
            writer,
            Arc::clone(&selected_payload),
            Arc::clone(&shutting_down),
            permit,
        )
        .expect("clipboard writer thread should start");

        // Let the pipe fill before replacing the selected source.
        thread::sleep(Duration::from_millis(25));
        selected_payload = Arc::from(&b"source-b"[..]);
        let mut received = Vec::new();
        File::from(reader)
            .read_to_end(&mut received)
            .expect("clipboard payload should drain from the pipe");
        handle
            .join()
            .expect("clipboard writer thread should finish cleanly");

        assert_eq!(received, &*payload_a);
        assert_eq!(&*selected_payload, b"source-b");
        assert_eq!(active.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn shutdown_cancels_a_blocked_write_and_releases_its_thread_slot() {
        let (reader_guard, writer) = pipe().expect("test pipe should open");
        set_nonblocking(&writer).expect("test pipe writer should become nonblocking");
        let active = Arc::new(AtomicUsize::new(0));
        let permit = TransferPermit::reserve(Arc::clone(&active), 1)
            .expect("first outgoing transfer slot should be available");
        let shutting_down = Arc::new(AtomicBool::new(false));
        let handle = spawn_write(
            writer,
            Arc::from(vec![0_u8; MAX_CLIPBOARD_BYTES]),
            Arc::clone(&shutting_down),
            permit,
        )
        .expect("clipboard writer thread should start");
        shutting_down.store(true, Ordering::SeqCst);
        handle
            .join()
            .expect("cancelled clipboard writer should finish cleanly");

        assert_eq!(active.load(Ordering::SeqCst), 0);
        assert!(TransferPermit::reserve(active, 1).is_some());
        drop(reader_guard);
    }

    #[test]
    fn completed_read_retries_a_full_queue_until_delivery() {
        let (sender, receiver) = calloop::channel::sync_channel(1);
        assert!(
            sender
                .try_send(ControlMessage::ClipboardReceived {
                    generation: 6,
                    result: Err("queue filler".to_owned()),
                })
                .is_ok()
        );
        let (reader, writer) = pipe().expect("test pipe should open");
        File::from(writer)
            .write_all(b"latest clipboard")
            .expect("test clipboard should be written");
        let active = Arc::new(AtomicUsize::new(0));
        let permit = TransferPermit::reserve(Arc::clone(&active), 1)
            .expect("first incoming transfer slot should be available");
        let current_generation = Arc::new(AtomicU64::new(7));
        let handle = spawn_read(reader, 7, Arc::clone(&current_generation), sender, permit)
            .expect("clipboard reader thread should start");

        thread::sleep(Duration::from_millis(25));
        let ControlMessage::ClipboardReceived {
            generation: filler_generation,
            ..
        } = receiver
            .try_recv()
            .expect("the queue filler should still occupy the queue");
        assert_eq!(filler_generation, 6);
        handle
            .join()
            .expect("clipboard reader thread should finish cleanly");

        let ControlMessage::ClipboardReceived { generation, result } = receiver
            .try_recv()
            .expect("the completed clipboard read should be retried");
        assert_eq!(generation, 7);
        assert_eq!(
            result.expect("the test clipboard should be valid").as_str(),
            "latest clipboard"
        );
        assert_eq!(active.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn superseding_a_completed_read_waiting_on_a_full_queue_releases_its_permit() {
        let (sender, receiver) = calloop::channel::sync_channel(1);
        assert!(
            sender
                .try_send(ControlMessage::ClipboardReceived {
                    generation: 10,
                    result: Err("queue filler".to_owned()),
                })
                .is_ok()
        );
        let (reader, writer) = pipe().expect("test pipe should open");
        File::from(writer)
            .write_all(b"superseded clipboard")
            .expect("test clipboard should be written");
        let active = Arc::new(AtomicUsize::new(0));
        let permit = TransferPermit::reserve(Arc::clone(&active), 1)
            .expect("first incoming transfer slot should be available");
        let current_generation = Arc::new(AtomicU64::new(11));
        let handle = spawn_read(
            reader,
            11,
            Arc::clone(&current_generation),
            sender.clone(),
            permit,
        )
        .expect("clipboard reader thread should start");

        thread::sleep(Duration::from_millis(25));
        current_generation.store(12, Ordering::SeqCst);
        handle
            .join()
            .expect("superseded clipboard reader should finish cleanly");

        assert_eq!(active.load(Ordering::SeqCst), 0);
        assert!(TransferPermit::reserve(Arc::clone(&active), 1).is_some());
        let ControlMessage::ClipboardReceived { generation, .. } = receiver
            .try_recv()
            .expect("the queue filler should remain queued");
        assert_eq!(generation, 10);
        assert!(matches!(
            receiver.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
    }

    #[test]
    fn disconnected_completion_queue_does_not_strand_a_read_permit() {
        let (sender, receiver) = calloop::channel::sync_channel(1);
        drop(receiver);
        let (reader, writer) = pipe().expect("test pipe should open");
        File::from(writer)
            .write_all(b"orphaned clipboard")
            .expect("test clipboard should be written");
        let active = Arc::new(AtomicUsize::new(0));
        let permit = TransferPermit::reserve(Arc::clone(&active), 1)
            .expect("first incoming transfer slot should be available");
        let current_generation = Arc::new(AtomicU64::new(13));
        let handle = spawn_read(reader, 13, current_generation, sender, permit)
            .expect("clipboard reader thread should start");

        handle
            .join()
            .expect("disconnected clipboard reader should finish cleanly");

        assert_eq!(active.load(Ordering::SeqCst), 0);
        assert!(TransferPermit::reserve(active, 1).is_some());
    }
}
