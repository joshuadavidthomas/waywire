use crate::{
    daemon::{CommandSink, Readiness},
    protocol::{DaemonCommand, FrameMetadata, VideoSample},
};
use anyhow::{Result, anyhow};
use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex},
};
use tokio::{
    net::UdpSocket,
    sync::{Notify, OwnedSemaphorePermit, Semaphore, mpsc},
    time::{Duration, Instant, sleep_until},
};

const MAX_ACCESS_UNIT: usize = 16 << 20;
const MAX_PENDING_RECORDS: usize = 120;
const MAX_PENDING_UNIT_BYTES: usize = 32 << 20;
const MAX_GOP_FRAMES: usize = 241;
const MAX_GOP_BYTES: usize = 32 << 20;
const MAX_VIEWER_FRAMES: usize = 8;
const MAX_VIEWER_BYTES: usize = 32 << 20;
const UNMATCHED_DEADLINE: Duration = Duration::from_secs(2);

#[derive(Clone)]
pub struct VideoHub {
    inner: Arc<Mutex<Hub>>,
}
struct Hub {
    subscribers: HashMap<u64, Subscriber>,
    next: u64,
    gop: Vec<VideoSample>,
    gop_bytes: usize,
}
struct Subscriber {
    queue: Arc<Mutex<ViewerQueue>>,
    notify: Arc<Notify>,
    waiting_for_keyframe: bool,
}
#[derive(Default)]
struct ViewerQueue {
    frames: VecDeque<VideoSample>,
    bytes: usize,
}
pub struct VideoSubscription {
    queue: Arc<Mutex<ViewerQueue>>,
    notify: Arc<Notify>,
}
impl VideoSubscription {
    pub async fn next(&self) -> VideoSample {
        loop {
            if let Some(frame) = {
                let mut queue = self.queue.lock().unwrap();
                let frame = queue.frames.pop_front();
                if let Some(value) = &frame {
                    queue.bytes -= value.data.len()
                }
                frame
            } {
                return frame;
            }
            self.notify.notified().await;
        }
    }
}
impl VideoHub {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Hub {
                subscribers: HashMap::new(),
                next: 1,
                gop: Vec::new(),
                gop_bytes: 0,
            })),
        }
    }
    pub fn subscribe(&self) -> (u64, Vec<VideoSample>, VideoSubscription) {
        let queue = Arc::new(Mutex::new(ViewerQueue::default()));
        let notify = Arc::new(Notify::new());
        let mut hub = self.inner.lock().unwrap();
        let id = hub.next;
        hub.next += 1;
        let bootstrap = hub.gop.clone();
        hub.subscribers.insert(
            id,
            Subscriber {
                queue: queue.clone(),
                notify: notify.clone(),
                waiting_for_keyframe: bootstrap.is_empty(),
            },
        );
        (id, bootstrap, VideoSubscription { queue, notify })
    }
    pub fn unsubscribe(&self, id: u64) {
        self.inner.lock().unwrap().subscribers.remove(&id);
    }
    fn broadcast(&self, sample: VideoSample) -> bool {
        let mut hub = self.inner.lock().unwrap();
        let stream_reset = sample.discontinuity
            || hub
                .gop
                .first()
                .is_some_and(|old| old.metadata.generation != sample.metadata.generation);
        if stream_reset {
            hub.gop.clear();
            hub.gop_bytes = 0;
        }
        if sample.key {
            hub.gop.clear();
            hub.gop_bytes = sample.data.len();
            hub.gop.push(sample.clone());
        } else if !hub.gop.is_empty() {
            if hub.gop.len() < MAX_GOP_FRAMES && hub.gop_bytes + sample.data.len() <= MAX_GOP_BYTES
            {
                hub.gop_bytes += sample.data.len();
                hub.gop.push(sample.clone());
            } else {
                hub.gop.clear();
                hub.gop_bytes = 0;
            }
        }
        for subscriber in hub.subscribers.values_mut() {
            let mut queue = subscriber.queue.lock().unwrap();
            if stream_reset {
                queue.frames.clear();
                queue.bytes = 0;
                subscriber.waiting_for_keyframe = true;
            }
            let recovering = subscriber.waiting_for_keyframe;
            if recovering {
                if !sample.key {
                    continue;
                }
                subscriber.waiting_for_keyframe = false;
            }
            let overflow = queue.frames.len() == MAX_VIEWER_FRAMES
                || queue.bytes + sample.data.len() > MAX_VIEWER_BYTES;
            if overflow {
                queue.frames.clear();
                queue.bytes = 0;
                if !sample.key {
                    subscriber.waiting_for_keyframe = true;
                    continue;
                }
                subscriber.waiting_for_keyframe = false;
            }
            let mut outgoing = sample.clone();
            if stream_reset || recovering || overflow {
                outgoing.discontinuity = true;
            }
            queue.bytes += outgoing.data.len();
            queue.frames.push_back(outgoing);
            drop(queue);
            subscriber.notify.notify_one();
        }
        let ready = hub.gop.first().is_some_and(|frame| {
            frame.key && frame.metadata.generation == sample.metadata.generation
        });
        drop(hub);
        ready
    }
}

#[derive(Debug)]
struct Packet {
    marker: bool,
    sequence: u16,
    timestamp: u32,
    ssrc: u32,
    payload: Vec<u8>,
}
fn decode_packet(data: &[u8]) -> Result<Packet> {
    if data.len() < 12 || data[0] >> 6 != 2 {
        return Err(anyhow!("invalid RTP header"));
    }
    let mut start = 12 + usize::from(data[0] & 15) * 4;
    if start > data.len() {
        return Err(anyhow!("truncated RTP CSRC list"));
    }
    if data[0] & 0x10 != 0 {
        if start + 4 > data.len() {
            return Err(anyhow!("truncated RTP extension"));
        }
        let words = usize::from(u16::from_be_bytes(
            data[start + 2..start + 4].try_into().unwrap(),
        ));
        start = start
            .checked_add(4 + words * 4)
            .ok_or_else(|| anyhow!("RTP extension overflow"))?;
        if start > data.len() {
            return Err(anyhow!("truncated RTP extension payload"));
        }
    }
    let mut end = data.len();
    if data[0] & 0x20 != 0 {
        let padding = usize::from(*data.last().unwrap());
        if padding == 0 || padding > end - start {
            return Err(anyhow!("invalid RTP padding"));
        }
        end -= padding;
    }
    if start == end || data[1] & 0x7f != 96 {
        return Err(anyhow!("wrong RTP payload type or empty payload"));
    }
    Ok(Packet {
        marker: data[1] & 0x80 != 0,
        sequence: u16::from_be_bytes(data[2..4].try_into().unwrap()),
        timestamp: u32::from_be_bytes(data[4..8].try_into().unwrap()),
        ssrc: u32::from_be_bytes(data[8..12].try_into().unwrap()),
        payload: data[start..end].to_vec(),
    })
}

#[derive(Default)]
struct Assembler {
    data: Vec<u8>,
    key: bool,
    fu_kind: Option<u8>,
    timestamp: Option<u32>,
    next_sequence: Option<u16>,
    ssrc: Option<u32>,
    recovering: bool,
}
#[derive(Debug)]
struct Unit {
    data: Arc<[u8]>,
    key: bool,
    discontinuity: bool,
    timestamp: u32,
    ssrc: u32,
}
impl Assembler {
    fn consume(&mut self, packet: Packet) -> Result<Option<Unit>> {
        if packet.ssrc == 0 {
            return Err(anyhow!("RTP SSRC generation must be positive"));
        }
        if self.ssrc.is_some_and(|ssrc| packet.ssrc < ssrc) {
            // SSRC is the monotonic frame generation. A packet from an older
            // encoder must not alter access-unit or restart state.
            return Ok(None);
        }
        if self.ssrc.is_some_and(|ssrc| packet.ssrc > ssrc) {
            self.clear_access_unit();
            self.next_sequence = None;
            self.recovering = true;
        }
        self.ssrc = Some(packet.ssrc);
        if self
            .next_sequence
            .is_some_and(|next| next != packet.sequence)
        {
            self.clear_access_unit();
            self.recovering = true;
        }
        self.next_sequence = Some(packet.sequence.wrapping_add(1));
        if self
            .timestamp
            .is_some_and(|timestamp| timestamp != packet.timestamp)
        {
            let incomplete = self.fu_kind.is_some();
            self.clear_access_unit();
            self.recovering = true;
            if incomplete {
                return Err(anyhow!("RTP timestamp changed during FU-A"));
            }
        }
        self.timestamp = Some(packet.timestamp);
        if let Err(error) = self.append_payload(&packet.payload) {
            self.clear_access_unit();
            self.recovering = true;
            return Err(error);
        }
        if !packet.marker {
            return Ok(None);
        }
        if self.fu_kind.is_some() {
            self.clear_access_unit();
            self.recovering = true;
            return Err(anyhow!("RTP marker ended an incomplete FU-A"));
        }
        let data = std::mem::take(&mut self.data);
        let key = std::mem::take(&mut self.key);
        self.timestamp = None;
        if data.is_empty() || self.recovering && !key {
            return Ok(None);
        }
        let discontinuity = std::mem::take(&mut self.recovering);
        Ok(Some(Unit {
            data: data.into(),
            key,
            discontinuity,
            timestamp: packet.timestamp,
            ssrc: packet.ssrc,
        }))
    }
    fn append_payload(&mut self, payload: &[u8]) -> Result<()> {
        if payload.is_empty() {
            return Err(anyhow!("empty H.264 RTP payload"));
        }
        let packet_type = payload[0] & 31;
        if self.fu_kind.is_some() && packet_type != 28 {
            return Err(anyhow!("NAL packet interleaved with FU-A"));
        }
        match packet_type {
            1..=23 => self.append_nal(payload),
            24 => {
                let mut rest = &payload[1..];
                while !rest.is_empty() {
                    if rest.len() < 2 {
                        return Err(anyhow!("truncated STAP-A length"));
                    }
                    let len = usize::from(u16::from_be_bytes(rest[..2].try_into().unwrap()));
                    rest = &rest[2..];
                    if len == 0 || len > rest.len() {
                        return Err(anyhow!("invalid STAP-A NAL length"));
                    }
                    self.append_nal(&rest[..len])?;
                    rest = &rest[len..];
                }
                Ok(())
            }
            28 => self.append_fu(payload),
            kind => Err(anyhow!("unsupported H.264 packetization type {kind}")),
        }
    }
    fn append_fu(&mut self, payload: &[u8]) -> Result<()> {
        if payload.len() < 3 {
            return Err(anyhow!("truncated FU-A payload"));
        }
        let start = payload[1] & 0x80 != 0;
        let end = payload[1] & 0x40 != 0;
        let reserved = payload[1] & 0x20 != 0;
        let kind = payload[1] & 31;
        if reserved || kind == 0 || kind > 23 || start && end {
            return Err(anyhow!("invalid FU-A header"));
        }
        match (start, self.fu_kind) {
            (true, None) => {
                self.push(&[0, 0, 0, 1, payload[0] & 0xe0 | kind])?;
                self.fu_kind = Some(kind);
            }
            (false, Some(active)) if active == kind => {}
            _ => return Err(anyhow!("invalid FU-A sequence")),
        }
        self.push(&payload[2..])?;
        if kind == 5 {
            self.key = true;
        }
        if end {
            self.fu_kind = None;
        }
        Ok(())
    }
    fn append_nal(&mut self, nal: &[u8]) -> Result<()> {
        if nal[0] & 31 == 5 {
            self.key = true;
        }
        self.push(&[0, 0, 0, 1])?;
        self.push(nal)
    }
    fn push(&mut self, value: &[u8]) -> Result<()> {
        if self.data.len().saturating_add(value.len()) > MAX_ACCESS_UNIT {
            return Err(anyhow!("H.264 access unit exceeds byte limit"));
        }
        self.data.extend_from_slice(value);
        Ok(())
    }
    fn clear_access_unit(&mut self) {
        self.data.clear();
        self.key = false;
        self.fu_kind = None;
        self.timestamp = None;
    }
}

struct Pending<T> {
    value: T,
    queued_at: Instant,
}
struct PendingUnit {
    unit: Unit,
    _bytes: OwnedSemaphorePermit,
}
struct Correlator {
    metadata: VecDeque<Pending<FrameMetadata>>,
    units: VecDeque<Pending<PendingUnit>>,
    last_timestamp: Option<u32>,
    last_generation: u32,
    last_fps: u32,
    ready_generation: u32,
}
impl Correlator {
    fn new() -> Self {
        Self {
            metadata: VecDeque::new(),
            units: VecDeque::new(),
            last_timestamp: None,
            last_generation: 0,
            last_fps: 60,
            ready_generation: 0,
        }
    }
    fn push_metadata(&mut self, value: FrameMetadata) -> Result<Vec<VideoSample>> {
        if value.generation == 0 {
            return Err(anyhow!("frame generation must be positive"));
        }
        if value.generation < self.last_generation {
            return self.drain();
        }
        if self.metadata.len() == MAX_PENDING_RECORDS {
            return Err(anyhow!("metadata count limit exceeded"));
        }
        self.metadata.push_back(Pending {
            value,
            queued_at: Instant::now(),
        });
        self.drain()
    }
    fn push_unit(&mut self, value: PendingUnit) -> Result<Vec<VideoSample>> {
        if value.unit.ssrc == 0 {
            return Err(anyhow!("RTP SSRC generation must be positive"));
        }
        if value.unit.ssrc < self.last_generation {
            return self.drain();
        }
        if self.units.len() == MAX_PENDING_RECORDS {
            return Err(anyhow!("RTP unit count limit exceeded"));
        }
        self.units.push_back(Pending {
            value,
            queued_at: Instant::now(),
        });
        self.drain()
    }
    fn drain(&mut self) -> Result<Vec<VideoSample>> {
        let mut samples = Vec::new();
        loop {
            while self
                .metadata
                .front()
                .is_some_and(|item| item.value.generation < self.last_generation)
            {
                self.metadata.pop_front();
            }
            let Some(pending) = self.units.front() else {
                break;
            };
            let unit = &pending.value.unit;
            if unit.ssrc < self.last_generation {
                self.units.pop_front();
                continue;
            }

            let same_generation = unit.ssrc == self.last_generation;
            let consume = if same_generation {
                metadata_gap(
                    unit.timestamp.wrapping_sub(self.last_timestamp.unwrap()),
                    self.last_fps,
                )?
            } else {
                1
            };
            let mut matches = 0;
            let mut metadata_index = None;
            let mut passed_generation = false;
            for (index, item) in self.metadata.iter().enumerate() {
                match item.value.generation.cmp(&unit.ssrc) {
                    std::cmp::Ordering::Less => {}
                    std::cmp::Ordering::Equal => {
                        matches += 1;
                        if matches == consume {
                            metadata_index = Some(index);
                            break;
                        }
                    }
                    std::cmp::Ordering::Greater => {
                        passed_generation = true;
                        break;
                    }
                }
            }
            let Some(index) = metadata_index else {
                if passed_generation {
                    // Metadata is ordered by generation. This unit's record can
                    // no longer arrive, so it cannot be correlated safely.
                    self.units.pop_front();
                    continue;
                }
                break;
            };

            let pending = self.units.pop_front().unwrap();
            let unit = pending.value.unit;
            let mut metadata = None;
            for _ in 0..=index {
                metadata = self.metadata.pop_front().map(|item| item.value);
            }
            let metadata = metadata.unwrap();
            debug_assert_eq!(metadata.generation, unit.ssrc);
            let generation_changed = metadata.generation != self.last_generation;
            let discontinuity = unit.discontinuity || index > 0 || generation_changed;
            self.last_timestamp = Some(unit.timestamp);
            self.last_generation = metadata.generation;
            self.last_fps = metadata.fps;
            samples.push(VideoSample {
                data: unit.data,
                key: unit.key,
                discontinuity,
                metadata,
            });
        }
        Ok(samples)
    }
    fn deadline(&self) -> Option<Instant> {
        self.metadata
            .front()
            .map(|v| v.queued_at)
            .into_iter()
            .chain(self.units.front().map(|v| v.queued_at))
            .min()
            .map(|at| at + UNMATCHED_DEADLINE)
    }
}
fn metadata_gap(delta: u32, fps: u32) -> Result<usize> {
    if delta == 0 {
        return Ok(1);
    }
    let count = (u64::from(delta) * u64::from(fps) + 45_000) / 90_000;
    if count == 0 {
        return Ok(1);
    }
    if count > MAX_PENDING_RECORDS as u64 {
        return Err(anyhow!("RTP timestamp gap exceeds correlation window"));
    }
    Ok(count as usize)
}

enum PipelineMessage {
    Metadata(FrameMetadata),
    Unit(PendingUnit),
}
#[derive(Clone)]
pub struct VideoPipeline {
    tx: mpsc::Sender<PipelineMessage>,
    unit_budget: Arc<Semaphore>,
}
pub struct VideoWorker {
    rx: mpsc::Receiver<PipelineMessage>,
    hub: VideoHub,
    commands: CommandSink,
    readiness: Readiness,
}
impl VideoPipeline {
    pub fn new(hub: VideoHub, commands: CommandSink, readiness: Readiness) -> (Self, VideoWorker) {
        let (tx, rx) = mpsc::channel(MAX_PENDING_RECORDS);
        let unit_budget = Arc::new(Semaphore::new(MAX_PENDING_UNIT_BYTES));
        (
            Self { tx, unit_budget },
            VideoWorker {
                rx,
                hub,
                commands,
                readiness,
            },
        )
    }
    pub async fn metadata(&self, value: FrameMetadata) -> Result<()> {
        self.tx
            .send(PipelineMessage::Metadata(value))
            .await
            .map_err(|_| anyhow!("video pipeline stopped"))
    }
    async fn unit(&self, value: Unit) -> Result<()> {
        let bytes =
            u32::try_from(value.data.len()).map_err(|_| anyhow!("access unit size overflow"))?;
        let permit = self
            .unit_budget
            .clone()
            .acquire_many_owned(bytes)
            .await
            .map_err(|_| anyhow!("video byte budget closed"))?;
        self.tx
            .send(PipelineMessage::Unit(PendingUnit {
                unit: value,
                _bytes: permit,
            }))
            .await
            .map_err(|_| anyhow!("video pipeline stopped"))
    }
    pub async fn receive(self, socket: UdpSocket) -> Result<()> {
        let mut assembler = Assembler::default();
        let mut buffer = vec![0; 65_536];
        loop {
            let (length, source) = socket.recv_from(&mut buffer).await?;
            if !source.ip().is_loopback() {
                continue;
            }
            let packet = match decode_packet(&buffer[..length]) {
                Ok(value) => value,
                Err(_) => continue,
            };
            if let Ok(Some(unit)) = assembler.consume(packet) {
                self.unit(unit).await?;
            }
        }
    }
}
impl VideoWorker {
    pub async fn run(mut self) -> Result<()> {
        let mut state = Correlator::new();
        loop {
            let message = if let Some(deadline) = state.deadline() {
                tokio::select! {
                    value = self.rx.recv() => value,
                    _ = sleep_until(deadline) => {
                        return Err(anyhow!("video metadata/RTP correlation stalled"));
                    }
                }
            } else {
                self.rx.recv().await
            };
            let Some(message) = message else {
                return Err(anyhow!("video pipeline closed"));
            };
            let frames = match message {
                PipelineMessage::Metadata(value) => state.push_metadata(value)?,
                PipelineMessage::Unit(value) => state.push_unit(value)?,
            };
            for frame in frames {
                let generation = frame.metadata.generation;
                let ready = self.hub.broadcast(frame);
                if ready {
                    self.readiness.mark_video_ready();
                } else {
                    self.readiness.await_keyframe();
                }
                let transition = if ready && state.ready_generation != generation {
                    state.ready_generation = generation;
                    Some((generation, true))
                } else if !ready && state.ready_generation != 0 {
                    Some((std::mem::take(&mut state.ready_generation), false))
                } else {
                    None
                };
                if let Some((generation, ready)) = transition {
                    self.commands
                        .system(DaemonCommand::KeyframeReadiness { generation, ready })
                        .await?;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn packet(sequence: u16, timestamp: u32, ssrc: u32, marker: bool, payload: &[u8]) -> Packet {
        Packet {
            marker,
            sequence,
            timestamp,
            ssrc,
            payload: payload.to_vec(),
        }
    }
    fn metadata(sequence: u64, generation: u32) -> FrameMetadata {
        FrameMetadata {
            generation,
            width: 1280,
            height: 720,
            capture_nanos: sequence,
            sequence,
            input_sequence: 0,
            fps: 30,
        }
    }
    fn unit(timestamp: u32, generation: u32) -> Unit {
        Unit {
            data: vec![1].into(),
            key: true,
            discontinuity: false,
            timestamp,
            ssrc: generation,
        }
    }
    #[test]
    fn emits_final_access_unit_at_marker() {
        let mut a = Assembler::default();
        assert!(
            a.consume(packet(1, 90, 7, false, &[0x7c, 0x85, 1]))
                .unwrap()
                .is_none()
        );
        let value = a
            .consume(packet(2, 90, 7, true, &[0x7c, 0x45, 2]))
            .unwrap()
            .unwrap();
        assert!(value.key);
        assert_eq!(&*value.data, [0, 0, 0, 1, 0x65, 1, 2]);
    }
    #[test]
    fn generation_restart_stays_sticky_across_fragments() {
        let mut a = Assembler::default();
        a.consume(packet(1, 1, 1, true, &[5, 1])).unwrap();
        assert!(
            a.consume(packet(2, 2, 2, false, &[0x7c, 0x85, 2]))
                .unwrap()
                .is_none()
        );
        let value = a
            .consume(packet(3, 2, 2, true, &[0x7c, 0x45, 3]))
            .unwrap()
            .unwrap();
        assert!(value.discontinuity);
    }
    #[test]
    fn rejects_invalid_fu_boundaries() {
        let mut a = Assembler::default();
        assert!(a.consume(packet(1, 1, 1, true, &[0x7c, 0xc5, 1])).is_err());
        assert!(
            a.consume(packet(2, 2, 1, false, &[0x7c, 0x85, 1]))
                .unwrap()
                .is_none()
        );
        assert!(a.consume(packet(3, 2, 1, true, &[0x7c, 0x41, 2])).is_err());
    }
    #[test]
    fn loss_and_sequence_wrap_recover_at_keyframe() {
        let mut a = Assembler::default();
        assert!(
            a.consume(packet(u16::MAX, 1, 1, true, &[1, 1]))
                .unwrap()
                .is_some()
        );
        assert!(a.consume(packet(0, 2, 1, true, &[1, 2])).unwrap().is_some());
        assert!(a.consume(packet(2, 3, 1, true, &[1, 3])).unwrap().is_none());
        assert!(
            a.consume(packet(3, 4, 1, true, &[5, 4]))
                .unwrap()
                .unwrap()
                .discontinuity
        );
    }
    fn pending(unit: Unit, budget: &Arc<Semaphore>) -> PendingUnit {
        PendingUnit {
            unit,
            _bytes: budget.clone().try_acquire_many_owned(1).unwrap(),
        }
    }

    #[test]
    fn first_unit_uses_only_metadata_with_its_generation() {
        let budget = Arc::new(Semaphore::new(MAX_PENDING_UNIT_BYTES));
        let mut correlator = Correlator::new();
        correlator.push_metadata(metadata(1, 1)).unwrap();
        correlator.push_metadata(metadata(2, 2)).unwrap();

        let samples = correlator.push_unit(pending(unit(1, 2), &budget)).unwrap();
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].metadata.sequence, 2);
        assert_eq!(samples[0].metadata.generation, 2);

        assert!(
            correlator
                .push_unit(pending(unit(2, 1), &budget))
                .unwrap()
                .is_empty()
        );
        assert!(correlator.units.is_empty());
    }

    #[test]
    fn reordered_replacements_never_exchange_metadata() {
        let budget = Arc::new(Semaphore::new(MAX_PENDING_UNIT_BYTES));
        let mut correlator = Correlator::new();
        correlator.push_metadata(metadata(1, 1)).unwrap();
        correlator
            .push_unit(pending(unit(3_000, 1), &budget))
            .unwrap();
        correlator.push_metadata(metadata(2, 2)).unwrap();
        correlator.push_metadata(metadata(3, 3)).unwrap();

        let samples = correlator.push_unit(pending(unit(1, 3), &budget)).unwrap();
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].metadata.sequence, 3);
        assert_eq!(samples[0].metadata.generation, 3);

        assert!(
            correlator
                .push_unit(pending(unit(1, 2), &budget))
                .unwrap()
                .is_empty()
        );
        assert!(correlator.units.is_empty());
    }

    #[test]
    fn old_packet_cannot_poison_current_assembler_state() {
        let budget = Arc::new(Semaphore::new(MAX_PENDING_UNIT_BYTES));
        let mut assembler = Assembler::default();
        let mut correlator = Correlator::new();

        correlator.push_metadata(metadata(1, 1)).unwrap();
        let first = assembler
            .consume(packet(1, 3_000, 1, true, &[5, 1]))
            .unwrap()
            .unwrap();
        correlator.push_unit(pending(first, &budget)).unwrap();

        correlator.push_metadata(metadata(2, 2)).unwrap();
        let replacement = assembler
            .consume(packet(1, 3_000, 2, true, &[5, 2]))
            .unwrap()
            .unwrap();
        correlator.push_unit(pending(replacement, &budget)).unwrap();
        correlator.push_metadata(metadata(3, 2)).unwrap();

        assert!(
            assembler
                .consume(packet(2, 6_000, 1, true, &[5, 3]))
                .unwrap()
                .is_none()
        );
        let current = assembler
            .consume(packet(2, 6_000, 2, true, &[5, 4]))
            .unwrap()
            .unwrap();
        assert!(!current.discontinuity);
        let samples = correlator.push_unit(pending(current, &budget)).unwrap();
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].metadata.sequence, 3);
        assert!(correlator.units.is_empty());
    }

    #[test]
    fn generation_change_does_not_apply_timestamp_gap_across_ssrcs() {
        let budget = Arc::new(Semaphore::new(MAX_PENDING_UNIT_BYTES));
        let mut correlator = Correlator::new();
        correlator.push_metadata(metadata(1, 1)).unwrap();
        correlator
            .push_unit(pending(unit(3_000, 1), &budget))
            .unwrap();
        correlator.push_metadata(metadata(2, 1)).unwrap();
        correlator.push_metadata(metadata(3, 2)).unwrap();

        let samples = correlator
            .push_unit(pending(unit(u32::MAX, 2), &budget))
            .unwrap();
        assert_eq!(samples.len(), 1);
        assert_eq!(samples[0].metadata.sequence, 3);
        assert!(samples[0].discontinuity);
        assert!(correlator.metadata.is_empty());
    }

    #[test]
    fn huge_timestamp_gap_fails_instead_of_guessing() {
        assert!(metadata_gap(u32::MAX, 60).is_err());
    }
    fn sample(sequence: u64, bytes: usize, key: bool) -> VideoSample {
        VideoSample {
            data: vec![1; bytes].into(),
            key,
            discontinuity: false,
            metadata: metadata(sequence, 1),
        }
    }

    #[test]
    fn viewer_queue_keeps_exact_frame_and_byte_bounds() {
        let hub = VideoHub::new();
        hub.broadcast(sample(0, 1, true));
        let (_, _, frames) = hub.subscribe();
        for sequence in 1..=MAX_VIEWER_FRAMES as u64 {
            hub.broadcast(sample(sequence, 1, sequence == MAX_VIEWER_FRAMES as u64));
        }
        assert_eq!(frames.queue.lock().unwrap().frames.len(), MAX_VIEWER_FRAMES);

        let hub = VideoHub::new();
        hub.broadcast(sample(0, 1, true));
        let (_, _, bytes) = hub.subscribe();
        hub.broadcast(sample(1, MAX_VIEWER_BYTES, true));
        assert_eq!(bytes.queue.lock().unwrap().bytes, MAX_VIEWER_BYTES);
    }

    #[test]
    fn overflow_keeps_arriving_keyframe_and_discards_delta() {
        let hub = VideoHub::new();
        hub.broadcast(sample(0, 1, true));
        let (_, _, slow) = hub.subscribe();
        let (_, _, independent) = hub.subscribe();
        for sequence in 1..=MAX_VIEWER_FRAMES as u64 {
            hub.broadcast(sample(sequence, 1, false));
        }
        independent.queue.lock().unwrap().frames.clear();
        independent.queue.lock().unwrap().bytes = 0;

        hub.broadcast(sample(20, 1, false));
        assert!(slow.queue.lock().unwrap().frames.is_empty());
        assert_eq!(independent.queue.lock().unwrap().frames.len(), 1);
        hub.broadcast(sample(21, 1, true));
        let slow_queue = slow.queue.lock().unwrap();
        assert_eq!(slow_queue.frames.len(), 1);
        assert!(slow_queue.frames[0].key && slow_queue.frames[0].discontinuity);
    }

    #[tokio::test]
    async fn viewer_starts_and_recovers_only_at_keyframe() {
        let hub = VideoHub::new();
        let (_, bootstrap, subscription) = hub.subscribe();
        assert!(bootstrap.is_empty());
        for n in 0..10 {
            hub.broadcast(VideoSample {
                data: vec![1].into(),
                key: false,
                discontinuity: false,
                metadata: metadata(n, 1),
            });
        }
        hub.broadcast(VideoSample {
            data: vec![1].into(),
            key: true,
            discontinuity: false,
            metadata: metadata(11, 1),
        });
        let value = subscription.next().await;
        assert!(value.key && value.discontinuity);
    }
}
