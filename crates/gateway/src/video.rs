use std::collections::HashMap;
use std::collections::VecDeque;
use std::fmt;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use sprite_desktop_protocol::browser::Continuity;
use sprite_desktop_protocol::browser::FrameKind;
use sprite_desktop_protocol::browser::MAX_VIDEO_DATA_BYTES;
use sprite_desktop_protocol::browser::VideoSample;
use sprite_desktop_protocol::pipe::Command;
use sprite_desktop_protocol::pipe::Fps;
use sprite_desktop_protocol::pipe::FrameMetadata;
use sprite_desktop_protocol::pipe::Generation;
use sprite_desktop_protocol::pipe::KeyframeReadiness;
use sprite_desktop_protocol::pipe::KeyframeState;
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

const MAX_ACCESS_UNIT: usize = MAX_VIDEO_DATA_BYTES;
const PENDING_RECORD_CAPACITY: usize = 120;
// Timestamp gaps wider than the queue cannot ever be matched.
const MAX_PENDING_RECORDS: u64 = PENDING_RECORD_CAPACITY as u64;
const MAX_PENDING_UNIT_BYTES: usize = 32 << 20;
const MAX_GOP_FRAMES: usize = 241;
const MAX_GOP_BYTES: usize = 32 << 20;
const VIEWER_BOUNDS: ViewerBounds = ViewerBounds {
    frames: 8,
    bytes: 32 << 20,
};
const UNMATCHED_DEADLINE: Duration = Duration::from_secs(2);
const DROP_REPORT_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Clone)]
pub(crate) struct VideoHub {
    inner: Arc<Mutex<Hub>>,
}

struct Hub {
    subscribers: HashMap<SubscriberId, ViewerHandle>,
    next_subscriber: u64,
    gop: Vec<VideoSample>,
    gop_bytes: usize,
    generation: Option<Generation>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct SubscriberId(u64);

#[derive(Clone)]
struct ViewerHandle {
    queue: Arc<Mutex<ViewerQueue>>,
    notify: Arc<Notify>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GopState {
    KeyframeCached,
    Recovering,
}

#[derive(Clone, Copy)]
struct ViewerBounds {
    frames: usize,
    bytes: usize,
}

struct ViewerQueue {
    frames: VecDeque<VideoSample>,
    bytes: usize,
    keyframe: GopState,
    generation: Option<Generation>,
    bounds: ViewerBounds,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Pushed {
    Queued,
    Dropped,
}

impl ViewerQueue {
    fn new(keyframe: GopState, generation: Option<Generation>, bounds: ViewerBounds) -> Self {
        Self {
            frames: VecDeque::new(),
            bytes: 0,
            keyframe,
            generation,
            bounds,
        }
    }

    fn push(&mut self, mut sample: VideoSample) -> Pushed {
        let generation_reset = self
            .generation
            .is_some_and(|current| current != sample.metadata.generation);
        self.generation = Some(sample.metadata.generation);
        let stream_reset = sample.continuity == Continuity::AfterGap || generation_reset;
        if stream_reset {
            self.clear();
        }
        let recovering = self.keyframe == GopState::Recovering;
        if recovering && sample.kind != FrameKind::Key {
            return Pushed::Dropped;
        }
        if sample.kind == FrameKind::Key {
            self.keyframe = GopState::KeyframeCached;
        }
        let overflow = self.frames.len() >= self.bounds.frames
            || self.bytes.saturating_add(sample.data.len()) > self.bounds.bytes;
        if overflow {
            debug!(
                frames = self.frames.len().saturating_add(1),
                bytes = self.bytes.saturating_add(sample.data.len()),
                kind = ?sample.kind,
                "viewer queue overflow"
            );
            self.clear();
            if sample.kind != FrameKind::Key {
                return Pushed::Dropped;
            }
            self.keyframe = GopState::KeyframeCached;
        }
        if stream_reset || recovering || overflow {
            sample.continuity = Continuity::AfterGap;
        }
        self.bytes += sample.data.len();
        self.frames.push_back(sample);
        Pushed::Queued
    }

    fn pop(&mut self) -> Option<VideoSample> {
        let frame = self.frames.pop_front();
        if let Some(value) = &frame {
            self.bytes -= value.data.len();
        }
        frame
    }

    fn clear(&mut self) {
        self.frames.clear();
        self.bytes = 0;
        self.keyframe = GopState::Recovering;
    }
}

pub(crate) struct VideoSubscription {
    hub: VideoHub,
    id: SubscriberId,
    bootstrap: VecDeque<VideoSample>,
    queue: Arc<Mutex<ViewerQueue>>,
    notify: Arc<Notify>,
}

impl VideoSubscription {
    pub(crate) async fn next(&mut self) -> VideoSample {
        loop {
            if let Some(frame) = self.bootstrap.pop_front() {
                let current_generation = self
                    .queue
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .generation;
                if current_generation == Some(frame.metadata.generation) {
                    return frame;
                }
                continue;
            }
            let notified = self.notify.notified();
            if let Some(frame) = self
                .queue
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .pop()
            {
                return frame;
            }
            // All durable progress lives in the queues. Cancellation can only
            // discard this waiter, and the next call checks both queues before waiting again.
            notified.await;
        }
    }
}

impl Drop for VideoSubscription {
    fn drop(&mut self) {
        self.hub.unsubscribe(self.id);
    }
}

impl VideoHub {
    pub(crate) fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Hub {
                subscribers: HashMap::new(),
                next_subscriber: 1,
                gop: Vec::new(),
                gop_bytes: 0,
                generation: None,
            })),
        }
    }

    pub(crate) fn subscribe(&self) -> VideoSubscription {
        let mut hub = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        let id = SubscriberId(hub.next_subscriber);
        hub.next_subscriber = hub.next_subscriber.wrapping_add(1);
        let bootstrap = VecDeque::from(hub.gop.clone());
        let keyframe = if bootstrap.is_empty() {
            GopState::Recovering
        } else {
            GopState::KeyframeCached
        };
        let queue = Arc::new(Mutex::new(ViewerQueue::new(
            keyframe,
            hub.generation,
            VIEWER_BOUNDS,
        )));
        let notify = Arc::new(Notify::new());
        let _ = hub.subscribers.insert(
            id,
            ViewerHandle {
                queue: Arc::clone(&queue),
                notify: Arc::clone(&notify),
            },
        );
        drop(hub);
        VideoSubscription {
            hub: self.clone(),
            id,
            bootstrap,
            queue,
            notify,
        }
    }

    fn unsubscribe(&self, id: SubscriberId) {
        self.inner
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .subscribers
            .remove(&id);
    }

    // VideoWorker::run is the sole production caller. Fan-out happens without the hub lock, so
    // per-viewer ordering depends on that caller broadcasting each frame in sequence.
    fn broadcast(&self, sample: &VideoSample) -> GopState {
        let generation = sample.metadata.generation;
        let (handles, state, stream_reset) = {
            let mut hub = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
            let stream_reset = sample.continuity == Continuity::AfterGap
                || hub
                    .generation
                    .is_some_and(|current| current != sample.metadata.generation);
            hub.generation = Some(sample.metadata.generation);
            if stream_reset {
                hub.gop.clear();
                hub.gop_bytes = 0;
            }
            if sample.kind == FrameKind::Key {
                hub.gop.clear();
                hub.gop_bytes = sample.data.len();
                hub.gop.push(sample.clone());
            } else if !hub.gop.is_empty() {
                if hub.gop.len() < MAX_GOP_FRAMES
                    && hub.gop_bytes + sample.data.len() <= MAX_GOP_BYTES
                {
                    hub.gop_bytes += sample.data.len();
                    hub.gop.push(sample.clone());
                } else {
                    hub.gop.clear();
                    hub.gop_bytes = 0;
                }
            }
            let state = if hub.gop.first().is_some_and(|frame| {
                frame.kind == FrameKind::Key && frame.metadata.generation == generation
            }) {
                GopState::KeyframeCached
            } else {
                GopState::Recovering
            };
            (
                hub.subscribers.values().cloned().collect::<Vec<_>>(),
                state,
                stream_reset,
            )
        };

        for handle in handles {
            let mut outgoing = sample.clone();
            if stream_reset {
                outgoing.continuity = Continuity::AfterGap;
            }
            let pushed = handle
                .queue
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .push(outgoing);
            match pushed {
                Pushed::Queued => handle.notify.notify_one(),
                Pushed::Dropped => {}
            }
        }
        state
    }
}

#[derive(Debug)]
struct Packet {
    end: AccessUnitEnd,
    sequence: u16,
    timestamp: u32,
    generation: Generation,
    payload: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AccessUnitEnd {
    Final,
    More,
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
        end: if data[1] & 0x80 == 0 {
            AccessUnitEnd::More
        } else {
            AccessUnitEnd::Final
        },
        sequence: u16::from_be_bytes([data[2], data[3]]),
        timestamp: u32::from_be_bytes([data[4], data[5], data[6], data[7]]),
        generation,
        payload: data[start..end].to_vec(),
    })
}

struct Assembler {
    data: Vec<u8>,
    kind: FrameKind,
    fu_kind: Option<u8>,
    timestamp: Option<u32>,
    next_sequence: Option<u16>,
    generation: Option<Generation>,
    buffered_packets: u64,
    keyframe: GopState,
}

impl Default for Assembler {
    fn default() -> Self {
        Self {
            data: Vec::new(),
            kind: FrameKind::Delta,
            fu_kind: None,
            timestamp: None,
            next_sequence: None,
            generation: None,
            buffered_packets: 0,
            keyframe: GopState::KeyframeCached,
        }
    }
}

#[derive(Debug)]
struct Unit {
    data: Arc<[u8]>,
    kind: FrameKind,
    continuity: Continuity,
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
            self.keyframe = GopState::Recovering;
        }
        self.generation = Some(packet.generation);
        if self
            .next_sequence
            .is_some_and(|next| next != packet.sequence)
        {
            let discarded = self.clear_access_unit();
            drops.record(discarded, AssemblyDrop::SequenceDiscontinuity);
            self.keyframe = GopState::Recovering;
        }
        self.next_sequence = Some(packet.sequence.wrapping_add(1));
        if self
            .timestamp
            .is_some_and(|timestamp| timestamp != packet.timestamp)
        {
            let incomplete = self.fu_kind.is_some();
            let discarded = self.clear_access_unit();
            drops.record(discarded, AssemblyDrop::TimestampDiscontinuity);
            self.keyframe = GopState::Recovering;
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
            self.keyframe = GopState::Recovering;
            return AssemblyResult::rejected(error, drops);
        }
        self.buffered_packets = self.buffered_packets.saturating_add(1);
        if packet.end == AccessUnitEnd::More {
            return AssemblyResult::accepted(AssemblyOutcome::Incomplete, drops);
        }
        if self.fu_kind.is_some() {
            let buffered = self.clear_access_unit();
            drops.record(buffered.saturating_sub(1), AssemblyDrop::PacketRejection);
            self.keyframe = GopState::Recovering;
            return AssemblyResult::rejected(anyhow!("RTP marker ended an incomplete FU-A"), drops);
        }
        let data = std::mem::take(&mut self.data);
        let kind = std::mem::replace(&mut self.kind, FrameKind::Delta);
        let access_unit_packets = std::mem::take(&mut self.buffered_packets);
        self.timestamp = None;
        if data.is_empty() {
            drops.record(access_unit_packets, AssemblyDrop::EmptyAccessUnit);
            return AssemblyResult::accepted(AssemblyOutcome::Dropped, drops);
        }
        if self.keyframe == GopState::Recovering && kind != FrameKind::Key {
            drops.record(access_unit_packets, AssemblyDrop::RecoveringWithoutKeyframe);
            return AssemblyResult::accepted(AssemblyOutcome::Dropped, drops);
        }
        let continuity = if self.keyframe == GopState::Recovering {
            self.keyframe = GopState::KeyframeCached;
            Continuity::AfterGap
        } else {
            Continuity::Continuous
        };
        AssemblyResult::accepted(
            AssemblyOutcome::Complete(Unit {
                data: data.into(),
                kind,
                continuity,
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
            self.kind = FrameKind::Key;
        }
        if end {
            self.fu_kind = None;
        }
        Ok(())
    }

    fn append_nal(&mut self, nal: &[u8]) -> Result<()> {
        if nal[0] & 31 == 5 {
            self.kind = FrameKind::Key;
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
        self.kind = FrameKind::Delta;
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
            let continuity =
                if unit.continuity == Continuity::AfterGap || index > 0 || generation_changed {
                    Continuity::AfterGap
                } else {
                    Continuity::Continuous
                };
            self.last_timestamp = Some(unit.timestamp);
            self.last_generation = Some(metadata.generation);
            self.last_fps = Some(metadata.fps);
            samples.push(VideoSample {
                data: unit.data,
                kind: unit.kind,
                continuity,
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
    usize::try_from(count).context("bounded metadata gap exceeds usize")
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
                let transition = match self.hub.broadcast(&frame) {
                    GopState::KeyframeCached => {
                        self.readiness.mark_video_ready();
                        if ready_generation == Some(generation) {
                            None
                        } else {
                            ready_generation = Some(generation);
                            Some((generation, KeyframeState::Cached))
                        }
                    }
                    GopState::Recovering => {
                        self.readiness.await_keyframe();
                        ready_generation
                            .take()
                            .map(|generation| (generation, KeyframeState::Missing))
                    }
                };
                if let Some((generation, state)) = transition {
                    self.commands
                        .system(Command::KeyframeReadiness(KeyframeReadiness {
                            generation,
                            state,
                        }))
                        .await?;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
