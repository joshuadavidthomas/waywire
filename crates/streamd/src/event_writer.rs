use std::{
    collections::VecDeque,
    io::{self, Write},
    sync::{Arc, Condvar, Mutex},
    thread,
    time::{Duration, Instant},
};

use nix::fcntl::{FcntlArg, OFlag, fcntl};
use thiserror::Error;

use crate::protocol::Event;

const MAX_QUEUED_EVENTS: usize = 256;
const MAX_QUEUED_BYTES: usize = 4 * 1024 * 1024;
const WRITE_STALL_LIMIT: Duration = Duration::from_secs(2);

#[derive(Debug, Error)]
pub enum EventWriterError {
    #[error("stdout event queue is full")]
    QueueFull,
    #[error("stdout event encoding failed")]
    Encode(#[from] crate::protocol::ProtocolError),
    #[error("stdout event writer failed: {0}")]
    Write(String),
    #[error("stdout event writer stopped")]
    Stopped,
}

struct Queue {
    records: VecDeque<Vec<u8>>,
    bytes: usize,
    stopping: bool,
    failure: Option<String>,
}

struct Shared {
    queue: Mutex<Queue>,
    ready: Condvar,
}

#[derive(Clone)]
pub struct EventSink {
    shared: Arc<Shared>,
}

pub struct EventWriter {
    shared: Arc<Shared>,
    thread: Option<thread::JoinHandle<()>>,
}

impl EventWriter {
    pub fn start() -> io::Result<(Self, EventSink)> {
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
            .spawn(move || writer_main(thread_shared))?;
        Ok((
            Self {
                shared: Arc::clone(&shared),
                thread: Some(thread),
            },
            EventSink { shared },
        ))
    }

    pub fn failure(&self) -> Option<EventWriterError> {
        self.shared
            .queue
            .lock()
            .unwrap()
            .failure
            .clone()
            .map(EventWriterError::Write)
    }

    pub fn stop(mut self) {
        self.stop_inner();
    }

    fn stop_inner(&mut self) {
        {
            let mut queue = self.shared.queue.lock().unwrap();
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
    pub fn send(&self, event: Event) -> Result<(), EventWriterError> {
        let bytes = event.encode()?;
        let replaceable_kind = matches!(bytes.get(1), Some(4 | 5)).then(|| bytes[1]);
        let mut queue = self.shared.queue.lock().unwrap();
        if let Some(failure) = &queue.failure {
            return Err(EventWriterError::Write(failure.clone()));
        }
        if queue.stopping {
            return Err(EventWriterError::Stopped);
        }

        if let Some(kind) = replaceable_kind
            && let Some(index) = queue.records.iter().rposition(|record| record[1] == kind)
        {
            let replaced_len = queue.records[index].len();
            let proposed_bytes = queue
                .bytes
                .saturating_sub(replaced_len)
                .saturating_add(bytes.len());
            if proposed_bytes > MAX_QUEUED_BYTES {
                return Err(EventWriterError::QueueFull);
            }
            queue.records[index] = bytes;
            queue.bytes = proposed_bytes;
            return Ok(());
        }

        if queue.records.len() == MAX_QUEUED_EVENTS
            || queue.bytes.saturating_add(bytes.len()) > MAX_QUEUED_BYTES
        {
            return Err(EventWriterError::QueueFull);
        }
        queue.bytes += bytes.len();
        queue.records.push_back(bytes);
        self.shared.ready.notify_one();
        Ok(())
    }
}

fn writer_main(shared: Arc<Shared>) {
    let stdout = io::stdout();
    if let Ok(raw_flags) = fcntl(&stdout, FcntlArg::F_GETFL) {
        let flags = OFlag::from_bits_truncate(raw_flags);
        if let Err(error) = fcntl(&stdout, FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK)) {
            shared.queue.lock().unwrap().failure = Some(error.to_string());
            return;
        }
    } else {
        shared.queue.lock().unwrap().failure = Some("could not read stdout flags".into());
        return;
    }
    let mut output = stdout.lock();
    loop {
        let record = {
            let mut queue = shared.queue.lock().unwrap();
            while queue.records.is_empty() && !queue.stopping {
                queue = shared.ready.wait(queue).unwrap();
            }
            if queue.records.is_empty() && queue.stopping {
                return;
            }
            let record = queue.records.pop_front().unwrap();
            queue.bytes -= record.len();
            record
        };
        if let Err(error) = write_with_deadline(&mut output, &record) {
            let mut queue = shared.queue.lock().unwrap();
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
    use super::*;
    use crate::protocol::FrameMetadata;

    fn sink_with_queue(records: VecDeque<Vec<u8>>, bytes: usize) -> EventSink {
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
        sink.send(Event::Frame(FrameMetadata {
            generation: 1,
            width: 2,
            height: 2,
            capture_nanos: 1,
            sequence: 1,
            input_sequence: 0,
            fps: 60,
        }))
        .unwrap();
        sink.send(Event::CursorVisibility(false)).unwrap();
        sink.send(Event::CursorVisibility(true)).unwrap();
        let queue = shared.queue.lock().unwrap();
        assert_eq!(queue.records.len(), 2);
        assert_eq!(queue.records[0][1], 2);
        assert_eq!(queue.records[1], vec![2, 5, 0, 0, 1, 0, 0, 0, 1]);
    }

    #[test]
    fn replaceable_cursor_cannot_break_the_byte_budget() {
        let old = Event::CursorImage {
            width: 1,
            height: 1,
            hotspot_x: 0,
            hotspot_y: 0,
            bgra: vec![0; 4],
        }
        .encode()
        .unwrap();
        let filler_len = MAX_QUEUED_BYTES - old.len();
        let records = VecDeque::from([vec![0; filler_len], old.clone()]);
        let sink = sink_with_queue(records, MAX_QUEUED_BYTES);

        let result = sink.send(Event::CursorImage {
            width: 2,
            height: 2,
            hotspot_x: 0,
            hotspot_y: 0,
            bgra: vec![0; 16],
        });

        assert!(matches!(result, Err(EventWriterError::QueueFull)));
        let queue = sink.shared.queue.lock().unwrap();
        assert_eq!(queue.bytes, MAX_QUEUED_BYTES);
        assert_eq!(queue.records.back(), Some(&old));
    }
}
