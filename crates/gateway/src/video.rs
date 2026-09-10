use std::collections::HashMap;
use std::collections::VecDeque;
use std::fmt;
use std::sync::Arc;
use std::sync::Mutex;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use sprite_desktop_protocol::browser::VideoSample;
use sprite_desktop_protocol::pipe::Command;
use sprite_desktop_protocol::pipe::Fps;
use sprite_desktop_protocol::pipe::FrameMetadata;
use sprite_desktop_protocol::pipe::Generation;
use tokio::net::UdpSocket;
use tokio::sync::Notify;
use tokio::sync::OwnedSemaphorePermit;
use tokio::sync::Semaphore;
use tokio::sync::mpsc;
use tokio::time::Duration;
use tokio::time::Instant;
use tokio::time::sleep_until;
use tracing::debug;
use tracing::warn;

use crate::daemon::CommandSink;
use crate::daemon::Readiness;
use crate::daemon::lock;

const MAX_ACCESS_UNIT: usize = 16 << 20;
const MAX_PENDING_RECORDS: u64 = 120;
const PENDING_RECORD_CAPACITY: usize = 120;
const MAX_PENDING_UNIT_BYTES: usize = 32 << 20;
const MAX_GOP_FRAMES: usize = 241;
const MAX_GOP_BYTES: usize = 32 << 20;
const MAX_VIEWER_FRAMES: usize = 8;
const MAX_VIEWER_BYTES: usize = 32 << 20;
const UNMATCHED_DEADLINE: Duration = Duration::from_secs(2);
const DROP_REPORT_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Clone)]
pub(crate) struct VideoHub {
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GopState {
    KeyframeCached,
    Recovering,
}

#[derive(Default)]
struct ViewerQueue {
    frames: VecDeque<VideoSample>,
    bytes: usize,
}

pub(crate) struct VideoSubscription {
    queue: Arc<Mutex<ViewerQueue>>,
    notify: Arc<Notify>,
}

impl VideoSubscription {
    pub(crate) async fn next(&self) -> VideoSample {
        loop {
            if let Some(frame) = {
                let mut queue = lock(&self.queue, "video viewer queue");
                let frame = queue.frames.pop_front();
                if let Some(value) = &frame {
                    queue.bytes -= value.data.len();
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
    pub(crate) fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Hub {
                subscribers: HashMap::new(),
                next: 1,
                gop: Vec::new(),
                gop_bytes: 0,
            })),
        }
    }

    pub(crate) fn subscribe(&self) -> (u64, Vec<VideoSample>, VideoSubscription) {
        let queue = Arc::new(Mutex::new(ViewerQueue::default()));
        let notify = Arc::new(Notify::new());
        let mut hub = lock(&self.inner, "video hub");
        let id = hub.next;
        hub.next += 1;
        let bootstrap = hub.gop.clone();
        hub.subscribers.insert(
            id,
            Subscriber {
                queue: Arc::clone(&queue),
                notify: Arc::clone(&notify),
                waiting_for_keyframe: bootstrap.is_empty(),
            },
        );
        (id, bootstrap, VideoSubscription { queue, notify })
    }

    pub(crate) fn unsubscribe(&self, id: u64) {
        let _ = lock(&self.inner, "video hub").subscribers.remove(&id);
    }

    fn broadcast(&self, sample: VideoSample) -> GopState {
        let generation = sample.metadata.generation;
        let mut hub = lock(&self.inner, "video hub");
        let stream_reset = sample.discontinuity
            || hub.gop.first().is_some_and(|first_cached| {
                first_cached.metadata.generation != sample.metadata.generation
            });
        if stream_reset {
            hub.gop.clear();
            hub.gop_bytes = 0;
        }
        for subscriber in hub.subscribers.values_mut() {
            {
                let mut queue = lock(&subscriber.queue, "video viewer queue");
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
            }
            subscriber.notify.notify_one();
        }
        if sample.key {
            hub.gop.clear();
            hub.gop_bytes = sample.data.len();
            hub.gop.push(sample);
        } else if !hub.gop.is_empty() {
            if hub.gop.len() < MAX_GOP_FRAMES && hub.gop_bytes + sample.data.len() <= MAX_GOP_BYTES
            {
                hub.gop_bytes += sample.data.len();
                hub.gop.push(sample);
            } else {
                hub.gop.clear();
                hub.gop_bytes = 0;
            }
        }
        if hub
            .gop
            .first()
            .is_some_and(|frame| frame.key && frame.metadata.generation == generation)
        {
            GopState::KeyframeCached
        } else {
            GopState::Recovering
        }
    }
}

#[derive(Debug)]
struct Packet {
    marker: bool,
    sequence: u16,
    timestamp: u32,
    generation: Generation,
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
        let words = usize::from(u16::from_be_bytes([data[start + 2], data[start + 3]]));
        start = start
            .checked_add(4 + words * 4)
            .ok_or_else(|| anyhow!("RTP extension overflow"))?;
        if start > data.len() {
            return Err(anyhow!("truncated RTP extension payload"));
        }
    }
    let mut end = data.len();
    if data[0] & 0x20 != 0 {
        let padding = usize::from(data[data.len() - 1]);
        if padding == 0 || padding > end - start {
            return Err(anyhow!("invalid RTP padding"));
        }
        end -= padding;
    }
    if start == end || data[1] & 0x7f != 96 {
        return Err(anyhow!("wrong RTP payload type or empty payload"));
    }
    let generation = Generation::new(u32::from_be_bytes([data[8], data[9], data[10], data[11]]))
        .map_err(|_invalid_generation| anyhow!("RTP SSRC generation must be positive"))?;
    Ok(Packet {
        marker: data[1] & 0x80 != 0,
        sequence: u16::from_be_bytes([data[2], data[3]]),
        timestamp: u32::from_be_bytes([data[4], data[5], data[6], data[7]]),
        generation,
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
    generation: Option<Generation>,
    buffered_packets: u64,
    recovering: bool,
}

#[derive(Debug)]
struct Unit {
    data: Arc<[u8]>,
    key: bool,
    discontinuity: bool,
    timestamp: u32,
    generation: Generation,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AssemblyDrop {
    EmptyAccessUnit,
    GenerationDiscontinuity,
    PacketRejection,
    RecoveringWithoutKeyframe,
    SequenceDiscontinuity,
    StaleGeneration,
    TimestampDiscontinuity,
}

impl fmt::Display for AssemblyDrop {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptyAccessUnit => formatter.write_str("empty RTP access unit"),
            Self::GenerationDiscontinuity => {
                formatter.write_str("RTP generation changed during a buffered access unit")
            }
            Self::PacketRejection => {
                formatter.write_str("buffered RTP access unit discarded after packet rejection")
            }
            Self::RecoveringWithoutKeyframe => {
                formatter.write_str("RTP access unit dropped while awaiting a keyframe")
            }
            Self::SequenceDiscontinuity => {
                formatter.write_str("RTP sequence changed during a buffered access unit")
            }
            Self::StaleGeneration => formatter.write_str("stale RTP generation"),
            Self::TimestampDiscontinuity => {
                formatter.write_str("RTP timestamp changed during a buffered access unit")
            }
        }
    }
}

#[derive(Debug)]
enum AssemblyOutcome {
    Incomplete,
    Complete(Unit),
    Dropped,
}

#[derive(Debug, Default)]
struct AssemblyDrops {
    packets: u64,
    last_reason: Option<AssemblyDrop>,
}

impl AssemblyDrops {
    fn record(&mut self, packets: u64, reason: AssemblyDrop) {
        if packets == 0 {
            return;
        }
        self.packets = self.packets.saturating_add(packets);
        self.last_reason = Some(reason);
    }
}

#[derive(Debug)]
struct AssemblyResult {
    outcome: Result<AssemblyOutcome>,
    drops: AssemblyDrops,
}

impl AssemblyResult {
    fn accepted(outcome: AssemblyOutcome, drops: AssemblyDrops) -> Self {
        Self {
            outcome: Ok(outcome),
            drops,
        }
    }

    fn rejected(error: anyhow::Error, drops: AssemblyDrops) -> Self {
        Self {
            outcome: Err(error),
            drops,
        }
    }
}

impl Assembler {
    fn consume(&mut self, packet: Packet) -> AssemblyResult {
        let mut drops = AssemblyDrops::default();
        if self
            .generation
            .is_some_and(|generation| packet.generation.get() < generation.get())
        {
            // SSRC is the monotonic frame generation. A packet from an older
            // encoder must not alter access-unit or restart state.
            drops.record(1, AssemblyDrop::StaleGeneration);
            return AssemblyResult::accepted(AssemblyOutcome::Dropped, drops);
        }
        if self
            .generation
            .is_some_and(|generation| packet.generation.get() > generation.get())
        {
            let discarded = self.clear_access_unit();
            drops.record(discarded, AssemblyDrop::GenerationDiscontinuity);
            self.next_sequence = None;
            self.recovering = true;
        }
        self.generation = Some(packet.generation);
        if self
            .next_sequence
            .is_some_and(|next| next != packet.sequence)
        {
            let discarded = self.clear_access_unit();
            drops.record(discarded, AssemblyDrop::SequenceDiscontinuity);
            self.recovering = true;
        }
        self.next_sequence = Some(packet.sequence.wrapping_add(1));
        if self
            .timestamp
            .is_some_and(|timestamp| timestamp != packet.timestamp)
        {
            let incomplete = self.fu_kind.is_some();
            let discarded = self.clear_access_unit();
            drops.record(discarded, AssemblyDrop::TimestampDiscontinuity);
            self.recovering = true;
            if incomplete {
                return AssemblyResult::rejected(
                    anyhow!("RTP timestamp changed during FU-A"),
                    drops,
                );
            }
        }
        self.timestamp = Some(packet.timestamp);
        let payload = packet.payload;
        if let Err(error) = self.append_payload(&payload) {
            let discarded = self.clear_access_unit();
            drops.record(discarded, AssemblyDrop::PacketRejection);
            self.recovering = true;
            return AssemblyResult::rejected(error, drops);
        }
        self.buffered_packets = self.buffered_packets.saturating_add(1);
        if !packet.marker {
            return AssemblyResult::accepted(AssemblyOutcome::Incomplete, drops);
        }
        if self.fu_kind.is_some() {
            let buffered = self.clear_access_unit();
            drops.record(buffered.saturating_sub(1), AssemblyDrop::PacketRejection);
            self.recovering = true;
            return AssemblyResult::rejected(anyhow!("RTP marker ended an incomplete FU-A"), drops);
        }
        let data = std::mem::take(&mut self.data);
        let key = std::mem::take(&mut self.key);
        let access_unit_packets = std::mem::take(&mut self.buffered_packets);
        self.timestamp = None;
        if data.is_empty() {
            drops.record(access_unit_packets, AssemblyDrop::EmptyAccessUnit);
            return AssemblyResult::accepted(AssemblyOutcome::Dropped, drops);
        }
        if self.recovering && !key {
            drops.record(access_unit_packets, AssemblyDrop::RecoveringWithoutKeyframe);
            return AssemblyResult::accepted(AssemblyOutcome::Dropped, drops);
        }
        let discontinuity = std::mem::take(&mut self.recovering);
        AssemblyResult::accepted(
            AssemblyOutcome::Complete(Unit {
                data: data.into(),
                key,
                discontinuity,
                timestamp: packet.timestamp,
                generation: packet.generation,
            }),
            drops,
        )
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
                    let len = usize::from(u16::from_be_bytes([rest[0], rest[1]]));
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

    fn clear_access_unit(&mut self) -> u64 {
        self.data.clear();
        self.key = false;
        self.fu_kind = None;
        self.timestamp = None;
        std::mem::take(&mut self.buffered_packets)
    }
}

struct Pending<T> {
    value: T,
    queued_at: Instant,
}

struct PendingUnit {
    unit: Unit,
    bytes: OwnedSemaphorePermit,
}

struct Correlator {
    metadata: VecDeque<Pending<FrameMetadata>>,
    units: VecDeque<Pending<PendingUnit>>,
    last_timestamp: Option<u32>,
    last_generation: Option<Generation>,
    last_fps: Option<Fps>,
}

impl Correlator {
    fn new() -> Self {
        Self {
            metadata: VecDeque::new(),
            units: VecDeque::new(),
            last_timestamp: None,
            last_generation: None,
            last_fps: None,
        }
    }

    fn push_metadata(&mut self, value: FrameMetadata) -> Result<Vec<VideoSample>> {
        if self
            .last_generation
            .is_some_and(|generation| value.generation.get() < generation.get())
        {
            return self.drain();
        }
        if self.metadata.len() == PENDING_RECORD_CAPACITY {
            return Err(anyhow!("metadata count limit exceeded"));
        }
        self.metadata.push_back(Pending {
            value,
            queued_at: Instant::now(),
        });
        self.drain()
    }

    fn push_unit(&mut self, value: PendingUnit) -> Result<Vec<VideoSample>> {
        if self
            .last_generation
            .is_some_and(|generation| value.unit.generation.get() < generation.get())
        {
            return self.drain();
        }
        if self.units.len() == PENDING_RECORD_CAPACITY {
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
            self.discard_stale_metadata();
            let Some(pending) = self.units.front() else {
                break;
            };
            let unit = &pending.value.unit;
            if self
                .last_generation
                .is_some_and(|generation| unit.generation.get() < generation.get())
            {
                let _ = self.units.pop_front();
                continue;
            }

            let metadata_count = if self.last_generation == Some(unit.generation) {
                let last_timestamp = self
                    .last_timestamp
                    .ok_or_else(|| anyhow!("current generation has no prior RTP timestamp"))?;
                let last_fps = self
                    .last_fps
                    .ok_or_else(|| anyhow!("current generation has no prior frame rate"))?;
                metadata_gap(unit.timestamp.wrapping_sub(last_timestamp), last_fps)?
            } else {
                1
            };
            let (metadata_index, passed_generation) =
                self.find_nth_generation_metadata(unit.generation, metadata_count);
            let Some(index) = metadata_index else {
                if passed_generation {
                    // Metadata is ordered by generation. This unit's record can
                    // no longer arrive, so it cannot be correlated safely.
                    let _ = self.units.pop_front();
                    continue;
                }
                break;
            };

            let pending = self
                .units
                .pop_front()
                .ok_or_else(|| anyhow!("front RTP unit disappeared during correlation"))?;
            let PendingUnit {
                unit,
                bytes: byte_permit,
            } = pending.value;
            drop(byte_permit);
            let metadata = self.take_metadata_through(index)?;
            debug_assert_eq!(metadata.generation, unit.generation);
            let generation_changed = self.last_generation != Some(metadata.generation);
            let discontinuity = unit.discontinuity || index > 0 || generation_changed;
            self.last_timestamp = Some(unit.timestamp);
            self.last_generation = Some(metadata.generation);
            self.last_fps = Some(metadata.fps);
            samples.push(VideoSample {
                data: unit.data,
                key: unit.key,
                discontinuity,
                metadata,
            });
        }
        Ok(samples)
    }

    fn discard_stale_metadata(&mut self) {
        while self.metadata.front().is_some_and(|item| {
            self.last_generation
                .is_some_and(|generation| item.value.generation.get() < generation.get())
        }) {
            let _ = self.metadata.pop_front();
        }
    }

    fn find_nth_generation_metadata(
        &self,
        generation: Generation,
        count: usize,
    ) -> (Option<usize>, bool) {
        let mut matches = 0;
        for (index, item) in self.metadata.iter().enumerate() {
            match item.value.generation.get().cmp(&generation.get()) {
                std::cmp::Ordering::Less => {}
                std::cmp::Ordering::Equal => {
                    matches += 1;
                    if matches == count {
                        return (Some(index), false);
                    }
                }
                std::cmp::Ordering::Greater => return (None, true),
            }
        }
        (None, false)
    }

    fn take_metadata_through(&mut self, index: usize) -> Result<FrameMetadata> {
        let mut metadata = None;
        for _ in 0..=index {
            metadata = self.metadata.pop_front().map(|item| item.value);
        }
        metadata.ok_or_else(|| anyhow!("matched frame metadata disappeared during correlation"))
    }

    fn deadline(&self) -> Option<Instant> {
        self.metadata
            .front()
            .map(|pending| pending.queued_at)
            .into_iter()
            .chain(self.units.front().map(|pending| pending.queued_at))
            .min()
            .map(|at| at + UNMATCHED_DEADLINE)
    }
}

#[allow(clippy::cast_possible_truncation)] // `count` is bounded to 120 before the cast.
fn metadata_gap(delta: u32, fps: Fps) -> Result<usize> {
    if delta == 0 {
        return Ok(1);
    }
    let count = (u64::from(delta) * u64::from(fps.get()) + 45_000) / 90_000;
    if count == 0 {
        return Ok(1);
    }
    if count > MAX_PENDING_RECORDS {
        return Err(anyhow!("RTP timestamp gap exceeds correlation window"));
    }
    Ok(count as usize)
}

enum PipelineMessage {
    Metadata(FrameMetadata),
    Unit(PendingUnit),
}

#[derive(Default)]
struct PacketDropSummary {
    dropped_packets: u64,
    rejected_packets: u64,
    last_reason: Option<String>,
    last_report: Option<Instant>,
}

impl PacketDropSummary {
    fn record_drops(&mut self, packets: u64, reason: impl fmt::Display) {
        debug!(dropped_packets = packets, reason = %reason, "RTP packets dropped");
        self.dropped_packets = self.dropped_packets.saturating_add(packets);
        self.last_reason = Some(reason.to_string());
    }

    fn record_rejection(&mut self, reason: impl fmt::Display) {
        debug!(reason = %reason, "RTP packet rejected");
        self.rejected_packets = self.rejected_packets.saturating_add(1);
        self.last_reason = Some(reason.to_string());
    }

    fn deadline(&self) -> Option<Instant> {
        if self.dropped_packets == 0 && self.rejected_packets == 0 {
            None
        } else {
            Some(
                self.last_report
                    .map_or_else(Instant::now, |at| at + DROP_REPORT_INTERVAL),
            )
        }
    }

    fn report_if_due(&mut self) {
        let Some(deadline) = self.deadline() else {
            return;
        };
        if deadline > Instant::now() {
            return;
        }
        self.report();
    }

    fn report(&mut self) {
        let last_reason = self
            .last_reason
            .take()
            .unwrap_or_else(|| "unknown RTP packet failure".to_owned());
        warn!(
            dropped_packets = self.dropped_packets,
            rejected_packets = self.rejected_packets,
            %last_reason,
            "RTP packets dropped or rejected"
        );
        self.dropped_packets = 0;
        self.rejected_packets = 0;
        self.last_report = Some(Instant::now());
    }
}

#[derive(Clone)]
pub(crate) struct VideoPipeline {
    tx: mpsc::Sender<PipelineMessage>,
    unit_budget: Arc<Semaphore>,
}

pub(crate) struct VideoWorker {
    rx: mpsc::Receiver<PipelineMessage>,
    hub: VideoHub,
    commands: CommandSink,
    readiness: Readiness,
}

impl VideoPipeline {
    pub(crate) fn new(
        hub: VideoHub,
        commands: CommandSink,
        readiness: Readiness,
    ) -> (Self, VideoWorker) {
        let (tx, rx) = mpsc::channel(PENDING_RECORD_CAPACITY);
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

    pub(crate) async fn metadata(&self, value: FrameMetadata) -> Result<()> {
        self.tx
            .send(PipelineMessage::Metadata(value))
            .await
            .context("video pipeline stopped")
    }

    async fn unit(&self, value: Unit) -> Result<()> {
        let bytes = u32::try_from(value.data.len()).context("access unit size overflow")?;
        let permit = Arc::clone(&self.unit_budget)
            .acquire_many_owned(bytes)
            .await
            .context("video byte budget closed")?;
        self.tx
            .send(PipelineMessage::Unit(PendingUnit {
                unit: value,
                bytes: permit,
            }))
            .await
            .context("video pipeline stopped")
    }

    pub(crate) async fn receive(self, socket: UdpSocket) -> Result<()> {
        let mut assembler = Assembler::default();
        let mut summary = PacketDropSummary::default();
        let mut buffer = vec![0; 65_536];
        loop {
            summary.report_if_due();
            let received = if let Some(deadline) = summary.deadline() {
                tokio::select! {
                    value = socket.recv_from(&mut buffer) => Some(value),
                    () = sleep_until(deadline) => None,
                }
            } else {
                Some(socket.recv_from(&mut buffer).await)
            };
            let Some(received) = received else {
                summary.report();
                continue;
            };
            let (length, source) = received?;
            if !source.ip().is_loopback() {
                summary.record_rejection(format_args!("non-loopback RTP source {source}"));
                continue;
            }
            let packet = match decode_packet(&buffer[..length]) {
                Ok(packet) => packet,
                Err(error) => {
                    summary.record_rejection(error);
                    continue;
                }
            };
            let result = assembler.consume(packet);
            if let Some(reason) = result.drops.last_reason {
                summary.record_drops(result.drops.packets, reason);
            }
            match result.outcome {
                Ok(AssemblyOutcome::Complete(unit)) => self.unit(unit).await?,
                Ok(AssemblyOutcome::Dropped | AssemblyOutcome::Incomplete) => {}
                Err(error) => summary.record_rejection(error),
            }
        }
    }
}

impl VideoWorker {
    pub(crate) async fn run(mut self) -> Result<()> {
        let mut state = Correlator::new();
        let mut ready_generation = None;
        loop {
            let message = if let Some(deadline) = state.deadline() {
                tokio::select! {
                    value = self.rx.recv() => value,
                    () = sleep_until(deadline) => {
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
                let transition = match self.hub.broadcast(frame) {
                    GopState::KeyframeCached => {
                        self.readiness.mark_video_ready();
                        if ready_generation == Some(generation) {
                            None
                        } else {
                            ready_generation = Some(generation);
                            Some((generation, true))
                        }
                    }
                    GopState::Recovering => {
                        self.readiness.await_keyframe();
                        ready_generation
                            .take()
                            .map(|generation| (generation, false))
                    }
                };
                if let Some((generation, ready)) = transition {
                    self.commands
                        .system(Command::KeyframeReadiness { generation, ready })
                        .await?;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
