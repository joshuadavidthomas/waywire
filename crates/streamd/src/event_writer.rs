use std::collections::VecDeque;
use std::io;
use std::io::Write;
use std::sync::Arc;
use std::sync::Condvar;
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::thread;
use std::time::Duration;
use std::time::Instant;

use nix::fcntl::FcntlArg;
use nix::fcntl::OFlag;
use nix::fcntl::fcntl;
use sprite_desktop_protocol::Record;
use sprite_desktop_protocol::pipe::Event;
use thiserror::Error;

const MAX_QUEUED_EVENTS: usize = 256;
const MAX_QUEUED_BYTES: usize = 4 * 1024 * 1024;
const WRITE_STALL_LIMIT: Duration = Duration::from_secs(2);

#[derive(Debug, Error)]
pub(crate) enum EventWriterError {
    #[error("stdout event queue is full")]
    QueueFull,
    #[error("stdout event writer failed: {0}")]
    Write(String),
    #[error("stdout event writer stopped")]
    Stopped,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Replacement {
    CursorImage,
    CursorVisibility,
}

impl Replacement {
    fn for_event(event: &Event) -> Option<Self> {
        match event {
            Event::CursorImage(_) => Some(Self::CursorImage),
            Event::CursorVisibility(_) => Some(Self::CursorVisibility),
            Event::Clipboard(_) | Event::Frame(_) | Event::ResizeApplied(_) => None,
        }
    }
}

struct QueuedEvent {
    bytes: Vec<u8>,
    replacement: Option<Replacement>,
}

struct Queue {
    records: VecDeque<QueuedEvent>,
    bytes: usize,
    stopping: bool,
    failure: Option<String>,
}

struct Shared {
    queue: Mutex<Queue>,
    ready: Condvar,
}

fn lock_queue(queue: &Mutex<Queue>) -> MutexGuard<'_, Queue> {
    match queue.lock() {
        Ok(queue) => queue,
        Err(error) => panic!("event writer queue mutex poisoned: {error}"),
    }
}

fn wait_for_queue<'a>(ready: &Condvar, queue: MutexGuard<'a, Queue>) -> MutexGuard<'a, Queue> {
    match ready.wait(queue) {
        Ok(queue) => queue,
        Err(error) => panic!("event writer queue mutex poisoned while waiting: {error}"),
    }
}

#[derive(Clone)]
pub(crate) struct EventSink {
    shared: Arc<Shared>,
}

pub(crate) struct EventWriter {
    shared: Arc<Shared>,
    thread: Option<thread::JoinHandle<()>>,
}

impl EventWriter {
    pub(crate) fn start() -> io::Result<(Self, EventSink)> {
        let shared = Arc::new(Shared {
            queue: Mutex::new(Queue {
                records: VecDeque::new(),
                bytes: 0,
                stopping: false,
                failure: None,
            }),
            ready: Condvar::new(),
        });
        let thread_shared = Arc::clone(&shared);
        let thread = thread::Builder::new()
            .name("streamd-events".into())
            .spawn(move || writer_main(&thread_shared))?;
        Ok((
            Self {
                shared: Arc::clone(&shared),
                thread: Some(thread),
            },
            EventSink { shared },
        ))
    }

    pub(crate) fn failure(&self) -> Option<EventWriterError> {
        lock_queue(&self.shared.queue)
            .failure
            .clone()
            .map(EventWriterError::Write)
    }

    pub(crate) fn stop(mut self) {
        self.stop_inner();
    }

    fn stop_inner(&mut self) {
        {
            let mut queue = lock_queue(&self.shared.queue);
            queue.stopping = true;
            self.shared.ready.notify_one();
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for EventWriter {
    fn drop(&mut self) {
        self.stop_inner();
    }
}

impl EventSink {
    pub(crate) fn send(&self, event: &Event) -> Result<(), EventWriterError> {
        let replacement = Replacement::for_event(event);
        let bytes = event.encode();
        let mut queue = lock_queue(&self.shared.queue);
        if let Some(failure) = &queue.failure {
            return Err(EventWriterError::Write(failure.clone()));
        }
        if queue.stopping {
            return Err(EventWriterError::Stopped);
        }

        if let Some(replacement) = replacement
            && let Some(index) = queue
                .records
                .iter()
                .rposition(|record| record.replacement == Some(replacement))
        {
            let replaced_len = queue.records[index].bytes.len();
            let proposed_bytes = queue
                .bytes
                .saturating_sub(replaced_len)
                .saturating_add(bytes.len());
            if proposed_bytes > MAX_QUEUED_BYTES {
                return Err(EventWriterError::QueueFull);
            }
            queue.records[index] = QueuedEvent {
                bytes,
                replacement: Some(replacement),
            };
            queue.bytes = proposed_bytes;
            return Ok(());
        }

        if queue.records.len() == MAX_QUEUED_EVENTS
            || queue.bytes.saturating_add(bytes.len()) > MAX_QUEUED_BYTES
        {
            return Err(EventWriterError::QueueFull);
        }
        queue.bytes += bytes.len();
        queue.records.push_back(QueuedEvent { bytes, replacement });
        self.shared.ready.notify_one();
        Ok(())
    }
}

fn writer_main(shared: &Shared) {
    let stdout = io::stdout();
    if let Ok(raw_flags) = fcntl(&stdout, FcntlArg::F_GETFL) {
        let flags = OFlag::from_bits_truncate(raw_flags);
        if let Err(error) = fcntl(&stdout, FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK)) {
            lock_queue(&shared.queue).failure = Some(error.to_string());
            return;
        }
    } else {
        lock_queue(&shared.queue).failure = Some("could not read stdout flags".into());
        return;
    }
    let mut output = stdout.lock();
    loop {
        let record = {
            let mut queue = lock_queue(&shared.queue);
            while queue.records.is_empty() && !queue.stopping {
                queue = wait_for_queue(&shared.ready, queue);
            }
            match queue.records.pop_front() {
                Some(record) => {
                    queue.bytes -= record.bytes.len();
                    record.bytes
                }
                None => return,
            }
        };
        if let Err(error) = write_with_deadline(&mut output, &record) {
            let mut queue = lock_queue(&shared.queue);
            queue.failure = Some(error.to_string());
            queue.records.clear();
            queue.bytes = 0;
            return;
        }
    }
}

fn write_with_deadline(output: &mut impl Write, bytes: &[u8]) -> io::Result<()> {
    let started = Instant::now();
    let mut offset = 0;
    while offset < bytes.len() {
        match output.write(&bytes[offset..]) {
            Ok(0) => return Err(io::Error::new(io::ErrorKind::BrokenPipe, "stdout closed")),
            Ok(count) => offset += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                if started.elapsed() >= WRITE_STALL_LIMIT {
                    return Err(io::Error::new(io::ErrorKind::TimedOut, "stdout stalled"));
                }
                thread::sleep(Duration::from_millis(5));
            }
            Err(error) => return Err(error),
        }
    }
    output.flush()
}

#[cfg(test)]
mod tests {
    use sprite_desktop_protocol::pipe::CursorImage;
    use sprite_desktop_protocol::pipe::CursorSize;
    use sprite_desktop_protocol::pipe::CursorVisibility;
    use sprite_desktop_protocol::pipe::Fps;
    use sprite_desktop_protocol::pipe::FrameDimension;
    use sprite_desktop_protocol::pipe::FrameMetadata;
    use sprite_desktop_protocol::pipe::Generation;
    use sprite_desktop_protocol::pipe::Hotspot;

    use super::*;

    fn sink_with_queue(records: VecDeque<QueuedEvent>, bytes: usize) -> EventSink {
        EventSink {
            shared: Arc::new(Shared {
                queue: Mutex::new(Queue {
                    records,
                    bytes,
                    stopping: false,
                    failure: None,
                }),
                ready: Condvar::new(),
            }),
        }
    }

    #[test]
    fn replaceable_cursor_does_not_discard_required_metadata() {
        let sink = sink_with_queue(VecDeque::new(), 0);
        let shared = Arc::clone(&sink.shared);
        sink.send(&Event::Frame(FrameMetadata {
            generation: Generation::new(1).expect("test generation should be valid"),
            width: FrameDimension::new(2).expect("test frame width should be valid"),
            height: FrameDimension::new(2).expect("test frame height should be valid"),
            capture_nanos: 1,
            sequence: 1,
            input_sequence: None,
            fps: Fps::new(60).expect("test frame rate should be valid"),
        }))
        .expect("frame metadata should queue");
        sink.send(&Event::CursorVisibility(CursorVisibility::Hidden))
            .expect("cursor hide should queue");
        let visible = Event::CursorVisibility(CursorVisibility::Visible);
        sink.send(&visible).expect("cursor show should queue");
        let expected = visible.encode();
        let queue = shared
            .queue
            .lock()
            .expect("queue lock should not be poisoned");
        assert_eq!(queue.records.len(), 2);
        assert_eq!(queue.records[0].replacement, None);
        assert_eq!(queue.records[1].bytes, expected);
    }

    #[test]
    fn replaceable_cursor_cannot_break_the_byte_budget() {
        let old = Event::CursorImage(
            CursorImage::new(
                CursorSize::new(1, 1).expect("test cursor size should be valid"),
                Hotspot { x: 0, y: 0 },
                vec![0; 4],
            )
            .expect("test cursor image should be valid"),
        )
        .encode();
        let filler_len = MAX_QUEUED_BYTES - old.len();
        let records = VecDeque::from([
            QueuedEvent {
                bytes: vec![0; filler_len],
                replacement: None,
            },
            QueuedEvent {
                bytes: old.clone(),
                replacement: Some(Replacement::CursorImage),
            },
        ]);
        let sink = sink_with_queue(records, MAX_QUEUED_BYTES);

        let image = CursorImage::new(
            CursorSize::new(2, 2).expect("test cursor size should be valid"),
            Hotspot { x: 0, y: 0 },
            vec![0; 16],
        )
        .expect("test cursor image should be valid");
        let result = sink.send(&Event::CursorImage(image));

        assert!(matches!(result, Err(EventWriterError::QueueFull)));
        let queue = sink
            .shared
            .queue
            .lock()
            .expect("queue lock should not be poisoned");
        assert_eq!(queue.bytes, MAX_QUEUED_BYTES);
        assert_eq!(queue.records.back().map(|record| &record.bytes), Some(&old));
    }
}
