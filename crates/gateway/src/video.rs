use std::collections::HashMap;
use std::collections::VecDeque;
use std::fmt;
use std::net::SocketAddr;
use std::num::NonZeroU64;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::PoisonError;

use anyhow::Context;
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
use thiserror::Error;
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
const RTP_CLOCK_HZ: u32 = 90_000;
const DYNAMIC_PAYLOAD_TYPE: u8 = 96;
const PENDING_RECORD_CAPACITY: usize = 120;
const MAX_PENDING_UNIT_BYTES: usize = 32 << 20;
// At the protocol maximum this spans two seconds plus their leading keyframe. Production
// keyframes arrive four times per second, so an oversized GOP reaches the byte bound first.
const MAX_GOP_FRAMES: usize = 241;
const MAX_GOP_BYTES: usize = 32 << 20;
const VIEWER_BYTES: usize = 32 << 20;
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
    viewer_bounds: ViewerBounds,
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

/// A viewer can hold two GOPs at the configured maximum frame rate. The byte bound is the
/// same as the GOP cache and exceeds the encoder's 16 MiB maximum access unit.
#[derive(Clone, Copy)]
struct ViewerBounds {
    frames: usize,
    bytes: usize,
}

impl ViewerBounds {
    fn for_max_fps(fps: Fps) -> Self {
        let keyframe_interval = fps.keyframe_interval() as usize;
        Self {
            frames: keyframe_interval.saturating_mul(2),
            bytes: VIEWER_BYTES,
        }
    }

    #[cfg(test)]
    fn new(frames: usize, bytes: usize) -> Self {
        Self { frames, bytes }
    }
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
    queue: Arc<Mutex<ViewerQueue>>,
    notify: Arc<Notify>,
}

impl VideoSubscription {
    pub(crate) async fn next(&mut self) -> VideoSample {
        loop {
            let notified = self.notify.notified();
            if let Some(frame) = self
                .queue
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .pop()
            {
                return frame;
            }
            // All durable progress lives in the queue. Cancellation can only discard this
            // waiter, and the next call checks the queue before waiting again.
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
    pub(crate) fn new(max_fps: Fps) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Hub {
                subscribers: HashMap::new(),
                next_subscriber: 1,
                gop: Vec::new(),
                gop_bytes: 0,
                generation: None,
                viewer_bounds: ViewerBounds::for_max_fps(max_fps),
            })),
        }
    }

    pub(crate) fn subscribe(&self) -> VideoSubscription {
        let mut hub = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
        let id = SubscriberId(hub.next_subscriber);
        hub.next_subscriber = hub.next_subscriber.wrapping_add(1);
        let bounds = hub.viewer_bounds;
        let mut viewer_queue = ViewerQueue::new(GopState::Recovering, hub.generation, bounds);
        // This explicitly accepts a subscriber joining after resync but before the recovery
        // keyframe. It receives the pre-resync GOP, a valid picture existing viewers saw. At
        // most one cached GOP is seeded here, and the recovery keyframe clears it.
        for frame in &hub.gop {
            match viewer_queue.push(frame.clone()) {
                Pushed::Queued => {}
                Pushed::Dropped => {
                    debug!(
                        gop_frames = hub.gop.len(),
                        gop_bytes = hub.gop_bytes,
                        viewer_frame_bound = bounds.frames,
                        viewer_byte_bound = bounds.bytes,
                        "cached GOP exceeds viewer bootstrap bounds"
                    );
                    break;
                }
            }
        }
        let queue = Arc::new(Mutex::new(viewer_queue));
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RtpTimestamp(u32);

impl RtpTimestamp {
    fn elapsed_since(self, earlier: Self) -> RtpTicks {
        RtpTicks(self.0.wrapping_sub(earlier.0))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RtpTicks(u32);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct RtpSequence(u16);

impl RtpSequence {
    fn next(self) -> Self {
        Self(self.0.wrapping_add(1))
    }
}

#[derive(Debug)]
struct Packet {
    end: AccessUnitEnd,
    sequence: RtpSequence,
    timestamp: RtpTimestamp,
    generation: Generation,
    payload: Vec<u8>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AccessUnitEnd {
    Final,
    More,
}

#[derive(Clone, Debug, Error, PartialEq, Eq)]
enum PacketReject {
    #[error("invalid RTP header")]
    InvalidHeader,
    #[error("truncated RTP CSRC list")]
    TruncatedCsrc,
    #[error("truncated RTP extension")]
    TruncatedExtension,
    #[error("RTP extension overflow")]
    ExtensionOverflow,
    #[error("truncated RTP extension payload")]
    TruncatedExtensionPayload,
    #[error("invalid RTP padding")]
    InvalidPadding,
    #[error("wrong RTP payload type or empty payload")]
    WrongPayloadTypeOrEmptyPayload,
    #[error("RTP SSRC generation must be positive")]
    ZeroGeneration,
    #[error("non-loopback RTP source {address}")]
    NonLoopbackSource { address: SocketAddr },
    #[error("RTP timestamp changed during FU-A")]
    TimestampChangedDuringFu,
    #[error("empty H.264 RTP payload")]
    EmptyPayload,
    #[error("NAL packet interleaved with FU-A")]
    NalInterleavedWithFu,
    #[error("truncated STAP-A length")]
    TruncatedStapLength,
    #[error("invalid STAP-A NAL length")]
    InvalidStapNalLength,
    #[error("unsupported H.264 packetization type {kind}")]
    UnsupportedPacketization { kind: u8 },
    #[error("truncated FU-A payload")]
    TruncatedFuPayload,
    #[error("invalid FU-A header")]
    InvalidFuHeader,
    #[error("invalid FU-A sequence")]
    InvalidFuSequence,
    #[error("RTP marker ended an incomplete FU-A")]
    MarkerEndedIncompleteFu,
    #[error("H.264 access unit exceeds byte limit")]
    AccessUnitTooLarge,
}

fn decode_packet(data: &[u8]) -> Result<Packet, PacketReject> {
    if data.len() < 12 || data[0] >> 6 != 2 {
        return Err(PacketReject::InvalidHeader);
    }
    let mut start = 12 + usize::from(data[0] & 15) * 4;
    if start > data.len() {
        return Err(PacketReject::TruncatedCsrc);
    }
    if data[0] & 0x10 != 0 {
        if start + 4 > data.len() {
            return Err(PacketReject::TruncatedExtension);
        }
        let words = usize::from(u16::from_be_bytes([data[start + 2], data[start + 3]]));
        start = start
            .checked_add(4 + words * 4)
            .ok_or(PacketReject::ExtensionOverflow)?;
        if start > data.len() {
            return Err(PacketReject::TruncatedExtensionPayload);
        }
    }
    let mut end = data.len();
    if data[0] & 0x20 != 0 {
        let padding = usize::from(data[data.len() - 1]);
        if padding == 0 || padding > end - start {
            return Err(PacketReject::InvalidPadding);
        }
        end -= padding;
    }
    if start == end || data[1] & 0x7f != DYNAMIC_PAYLOAD_TYPE {
        return Err(PacketReject::WrongPayloadTypeOrEmptyPayload);
    }
    let generation = Generation::new(u32::from_be_bytes([data[8], data[9], data[10], data[11]]))
        .map_err(|_invalid_generation| PacketReject::ZeroGeneration)?;
    Ok(Packet {
        end: if data[1] & 0x80 == 0 {
            AccessUnitEnd::More
        } else {
            AccessUnitEnd::Final
        },
        sequence: RtpSequence(u16::from_be_bytes([data[2], data[3]])),
        timestamp: RtpTimestamp(u32::from_be_bytes([data[4], data[5], data[6], data[7]])),
        generation,
        payload: data[start..end].to_vec(),
    })
}

struct Assembler {
    data: Vec<u8>,
    kind: FrameKind,
    fu_kind: Option<u8>,
    timestamp: Option<RtpTimestamp>,
    next_sequence: Option<RtpSequence>,
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
    timestamp: RtpTimestamp,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DroppedPackets {
    packets: NonZeroU64,
    reason: AssemblyDrop,
}

impl DroppedPackets {
    fn record(slot: &mut Option<Self>, packets: NonZeroU64, reason: AssemblyDrop) {
        let packets = slot.map_or(packets, |dropped| {
            dropped.packets.saturating_add(packets.get())
        });
        *slot = Some(Self { packets, reason });
    }
}

#[derive(Debug)]
struct AssemblyResult {
    outcome: Result<AssemblyOutcome, PacketReject>,
    drops: Option<DroppedPackets>,
}

impl AssemblyResult {
    fn accepted(outcome: AssemblyOutcome, drops: Option<DroppedPackets>) -> Self {
        Self {
            outcome: Ok(outcome),
            drops,
        }
    }

    fn rejected(error: PacketReject, drops: Option<DroppedPackets>) -> Self {
        Self {
            outcome: Err(error),
            drops,
        }
    }
}

impl Assembler {
    fn consume(&mut self, packet: Packet) -> AssemblyResult {
        let mut drops = None;
        if self
            .generation
            .is_some_and(|generation| packet.generation.get() < generation.get())
        {
            // SSRC is the monotonic frame generation. A packet from an older
            // encoder must not alter access-unit or restart state.
            DroppedPackets::record(&mut drops, NonZeroU64::MIN, AssemblyDrop::StaleGeneration);
            return AssemblyResult::accepted(AssemblyOutcome::Dropped, drops);
        }
        if self
            .generation
            .is_some_and(|generation| packet.generation.get() > generation.get())
        {
            if let Some(packets) = self.clear_access_unit() {
                DroppedPackets::record(&mut drops, packets, AssemblyDrop::GenerationDiscontinuity);
            }
            self.next_sequence = None;
            self.keyframe = GopState::Recovering;
        }
        self.generation = Some(packet.generation);
        if self
            .next_sequence
            .is_some_and(|next| next != packet.sequence)
        {
            if let Some(packets) = self.clear_access_unit() {
                DroppedPackets::record(&mut drops, packets, AssemblyDrop::SequenceDiscontinuity);
            }
            self.keyframe = GopState::Recovering;
        }
        self.next_sequence = Some(packet.sequence.next());
        if self
            .timestamp
            .is_some_and(|timestamp| timestamp != packet.timestamp)
        {
            let incomplete = self.fu_kind.is_some();
            if let Some(packets) = self.clear_access_unit() {
                DroppedPackets::record(&mut drops, packets, AssemblyDrop::TimestampDiscontinuity);
            }
            self.keyframe = GopState::Recovering;
            if incomplete {
                return AssemblyResult::rejected(PacketReject::TimestampChangedDuringFu, drops);
            }
        }
        self.timestamp = Some(packet.timestamp);
        let payload = packet.payload;
        if let Err(error) = self.append_payload(&payload) {
            if let Some(packets) = self.clear_access_unit() {
                DroppedPackets::record(&mut drops, packets, AssemblyDrop::PacketRejection);
            }
            self.keyframe = GopState::Recovering;
            return AssemblyResult::rejected(error, drops);
        }
        self.buffered_packets = self.buffered_packets.saturating_add(1);
        if packet.end == AccessUnitEnd::More {
            return AssemblyResult::accepted(AssemblyOutcome::Incomplete, drops);
        }
        if self.fu_kind.is_some() {
            if let Some(buffered) = self.clear_access_unit()
                && let Some(packets) = NonZeroU64::new(buffered.get().saturating_sub(1))
            {
                DroppedPackets::record(&mut drops, packets, AssemblyDrop::PacketRejection);
            }
            self.keyframe = GopState::Recovering;
            return AssemblyResult::rejected(PacketReject::MarkerEndedIncompleteFu, drops);
        }
        let data = std::mem::take(&mut self.data);
        let kind = std::mem::replace(&mut self.kind, FrameKind::Delta);
        let access_unit_packets = std::mem::take(&mut self.buffered_packets);
        self.timestamp = None;
        if data.is_empty() {
            if let Some(packets) = NonZeroU64::new(access_unit_packets) {
                DroppedPackets::record(&mut drops, packets, AssemblyDrop::EmptyAccessUnit);
            }
            return AssemblyResult::accepted(AssemblyOutcome::Dropped, drops);
        }
        if self.keyframe == GopState::Recovering && kind != FrameKind::Key {
            if let Some(packets) = NonZeroU64::new(access_unit_packets) {
                DroppedPackets::record(
                    &mut drops,
                    packets,
                    AssemblyDrop::RecoveringWithoutKeyframe,
                );
            }
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

    fn append_payload(&mut self, payload: &[u8]) -> Result<(), PacketReject> {
        if payload.is_empty() {
            return Err(PacketReject::EmptyPayload);
        }
        let packet_type = payload[0] & 31;
        if self.fu_kind.is_some() && packet_type != 28 {
            return Err(PacketReject::NalInterleavedWithFu);
        }
        match packet_type {
            1..=23 => self.append_nal(payload),
            24 => {
                let mut rest = &payload[1..];
                while !rest.is_empty() {
                    if rest.len() < 2 {
                        return Err(PacketReject::TruncatedStapLength);
                    }
                    let len = usize::from(u16::from_be_bytes([rest[0], rest[1]]));
                    rest = &rest[2..];
                    if len == 0 || len > rest.len() {
                        return Err(PacketReject::InvalidStapNalLength);
                    }
                    self.append_nal(&rest[..len])?;
                    rest = &rest[len..];
                }
                Ok(())
            }
            28 => self.append_fu(payload),
            kind => Err(PacketReject::UnsupportedPacketization { kind }),
        }
    }

    fn append_fu(&mut self, payload: &[u8]) -> Result<(), PacketReject> {
        if payload.len() < 3 {
            return Err(PacketReject::TruncatedFuPayload);
        }
        let start = payload[1] & 0x80 != 0;
        let end = payload[1] & 0x40 != 0;
        let reserved = payload[1] & 0x20 != 0;
        let kind = payload[1] & 31;
        if reserved || kind == 0 || kind > 23 || start && end {
            return Err(PacketReject::InvalidFuHeader);
        }
        match (start, self.fu_kind) {
            (true, None) => {
                self.push(&[0, 0, 0, 1, payload[0] & 0xe0 | kind])?;
                self.fu_kind = Some(kind);
            }
            (false, Some(active)) if active == kind => {}
            _ => return Err(PacketReject::InvalidFuSequence),
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

    fn append_nal(&mut self, nal: &[u8]) -> Result<(), PacketReject> {
        if nal[0] & 31 == 5 {
            self.kind = FrameKind::Key;
        }
        self.push(&[0, 0, 0, 1])?;
        self.push(nal)
    }

    fn push(&mut self, value: &[u8]) -> Result<(), PacketReject> {
        if self.data.len().saturating_add(value.len()) > MAX_ACCESS_UNIT {
            return Err(PacketReject::AccessUnitTooLarge);
        }
        self.data.extend_from_slice(value);
        Ok(())
    }

    fn clear_access_unit(&mut self) -> Option<NonZeroU64> {
        self.data.clear();
        self.kind = FrameKind::Delta;
        self.fu_kind = None;
        self.timestamp = None;
        NonZeroU64::new(std::mem::take(&mut self.buffered_packets))
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

#[derive(Debug, PartialEq, Eq)]
enum MetadataMatch {
    Matched {
        metadata: FrameMetadata,
        unmatched: usize,
    },
    NotYetArrived,
    GenerationPassed,
}

#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
enum ResyncReason {
    #[error("metadata count limit exceeded")]
    MetadataBacklog,
    #[error("RTP unit count limit exceeded")]
    UnitBacklog,
    #[error("video metadata/RTP correlation stalled")]
    UnmatchedDeadline,
    #[error("RTP timestamp gap {ticks:?} exceeds correlation window at {fps:?}")]
    TimestampGap { ticks: RtpTicks, fps: Fps },
    #[error("metadata generation passed the pending RTP unit generation")]
    MetadataGenerationPassed,
}

#[must_use]
#[derive(Debug)]
struct Correlation {
    outcome: CorrelationOutcome,
    unmatched_metadata: usize,
}

#[derive(Debug)]
enum CorrelationOutcome {
    Frames(Vec<VideoSample>),
    Resync(ResyncReason),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TimestampAnchor {
    Needed,
    Present { timestamp: RtpTimestamp, fps: Fps },
}

struct Correlator {
    metadata: VecDeque<Pending<FrameMetadata>>,
    units: VecDeque<Pending<PendingUnit>>,
    last_generation: Option<Generation>,
    timestamp_anchor: TimestampAnchor,
    keyframe: GopState,
}

impl Correlator {
    fn new() -> Self {
        Self {
            metadata: VecDeque::new(),
            units: VecDeque::new(),
            last_generation: None,
            timestamp_anchor: TimestampAnchor::Needed,
            keyframe: GopState::KeyframeCached,
        }
    }

    fn push_metadata(&mut self, value: FrameMetadata) -> Correlation {
        if self.metadata.len() == PENDING_RECORD_CAPACITY {
            return self.resync(ResyncReason::MetadataBacklog, 1);
        }
        self.metadata.push_back(Pending {
            value,
            queued_at: Instant::now(),
        });
        self.drain()
    }

    fn push_unit(&mut self, value: PendingUnit) -> Correlation {
        if self.units.len() == PENDING_RECORD_CAPACITY {
            return self.resync(ResyncReason::UnitBacklog, 0);
        }
        self.units.push_back(Pending {
            value,
            queued_at: Instant::now(),
        });
        self.drain()
    }

    fn deadline_expired(&mut self) -> Correlation {
        self.resync(ResyncReason::UnmatchedDeadline, 0)
    }

    fn drain(&mut self) -> Correlation {
        let mut samples = Vec::new();
        let mut unmatched_metadata = 0_usize;
        loop {
            let Some(pending) = self.units.pop_front() else {
                break;
            };
            let unit = &pending.value.unit;
            let metadata_count = if self.last_generation == Some(unit.generation) {
                match self.timestamp_anchor {
                    TimestampAnchor::Needed => NonZeroUsize::MIN,
                    TimestampAnchor::Present { timestamp, fps } => {
                        let ticks = unit.timestamp.elapsed_since(timestamp);
                        let Some(gap) = metadata_gap(ticks, fps) else {
                            return self.resync(
                                ResyncReason::TimestampGap { ticks, fps },
                                unmatched_metadata,
                            );
                        };
                        gap
                    }
                }
            } else {
                NonZeroUsize::MIN
            };
            let matched = self.take_nth_generation_metadata(unit.generation, metadata_count);
            let (metadata, unmatched) = match matched {
                MetadataMatch::Matched {
                    metadata,
                    unmatched,
                } => (metadata, unmatched),
                MetadataMatch::NotYetArrived => {
                    self.units.push_front(pending);
                    break;
                }
                MetadataMatch::GenerationPassed => {
                    return self.resync(ResyncReason::MetadataGenerationPassed, unmatched_metadata);
                }
            };

            unmatched_metadata = unmatched_metadata.saturating_add(unmatched);
            let PendingUnit {
                unit,
                bytes: byte_permit,
            } = pending.value;
            drop(byte_permit);
            let generation_changed = self.last_generation != Some(metadata.generation);
            let recovering = self.keyframe == GopState::Recovering;
            let continuity = if recovering
                || unit.continuity == Continuity::AfterGap
                || unmatched > 0
                || generation_changed
            {
                Continuity::AfterGap
            } else {
                Continuity::Continuous
            };
            self.last_generation = Some(metadata.generation);
            self.timestamp_anchor = TimestampAnchor::Present {
                timestamp: unit.timestamp,
                fps: metadata.fps,
            };
            if recovering && unit.kind != FrameKind::Key {
                continue;
            }
            if recovering {
                self.keyframe = GopState::KeyframeCached;
            }
            samples.push(VideoSample {
                data: unit.data,
                kind: unit.kind,
                continuity,
                metadata,
            });
        }
        Correlation {
            outcome: CorrelationOutcome::Frames(samples),
            unmatched_metadata,
        }
    }

    fn resync(&mut self, reason: ResyncReason, unmatched_metadata: usize) -> Correlation {
        let unmatched_metadata = unmatched_metadata.saturating_add(self.metadata.len());
        self.metadata.clear();
        self.units.clear();
        self.timestamp_anchor = TimestampAnchor::Needed;
        self.keyframe = GopState::Recovering;
        Correlation {
            outcome: CorrelationOutcome::Resync(reason),
            unmatched_metadata,
        }
    }

    fn take_nth_generation_metadata(
        &mut self,
        generation: Generation,
        count: NonZeroUsize,
    ) -> MetadataMatch {
        let mut matches = 0;
        for (index, item) in self.metadata.iter().enumerate() {
            match item.value.generation.get().cmp(&generation.get()) {
                std::cmp::Ordering::Less => {}
                std::cmp::Ordering::Equal => {
                    matches += 1;
                    if matches == count.get() {
                        let metadata = item.value.clone();
                        drop(self.metadata.drain(..=index));
                        return MetadataMatch::Matched {
                            metadata,
                            unmatched: index,
                        };
                    }
                }
                std::cmp::Ordering::Greater => return MetadataMatch::GenerationPassed,
            }
        }
        MetadataMatch::NotYetArrived
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

fn metadata_gap(ticks: RtpTicks, fps: Fps) -> Option<NonZeroUsize> {
    if ticks.0 == 0 {
        return Some(NonZeroUsize::MIN);
    }
    let count = (u64::from(ticks.0) * u64::from(fps.get()) + u64::from(RTP_CLOCK_HZ / 2))
        / u64::from(RTP_CLOCK_HZ);
    if count == 0 {
        return Some(NonZeroUsize::MIN);
    }
    let count = usize::try_from(count).ok()?;
    if count > PENDING_RECORD_CAPACITY {
        return None;
    }
    NonZeroUsize::new(count)
}

enum PipelineMessage {
    Metadata(FrameMetadata),
    Unit(PendingUnit),
}

enum WorkerWake {
    Message(PipelineMessage),
    PipelineClosed,
    CorrelatorDeadline,
    MetadataReportDeadline,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum PacketFailure {
    Drop(AssemblyDrop),
    Reject(PacketReject),
}

impl fmt::Display for PacketFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Drop(reason) => reason.fmt(formatter),
            Self::Reject(reason) => reason.fmt(formatter),
        }
    }
}

#[derive(Debug)]
struct PacketDropReport {
    dropped: u64,
    rejected: u64,
    last_failure: PacketFailure,
}

#[derive(Debug)]
struct MetadataDropReport {
    unmatched: usize,
}

#[derive(Debug)]
struct DropSummary<R> {
    pending: Option<R>,
    last_report: Instant,
}

impl<R> DropSummary<R> {
    fn new() -> Self {
        Self {
            pending: None,
            last_report: Instant::now(),
        }
    }

    fn deadline(&self) -> Option<Instant> {
        self.pending
            .as_ref()
            .map(|_pending| self.last_report + DROP_REPORT_INTERVAL)
    }

    fn report_if_due(&mut self) -> Option<R> {
        let deadline = self.deadline()?;
        if deadline > Instant::now() {
            return None;
        }
        self.report()
    }

    fn report(&mut self) -> Option<R> {
        let report = self.pending.take();
        if report.is_some() {
            self.last_report = Instant::now();
        }
        report
    }

    fn force_due(&mut self) {
        self.last_report = Instant::now() - DROP_REPORT_INTERVAL;
    }
}

type PacketDropSummary = DropSummary<PacketDropReport>;

impl PacketDropSummary {
    fn record(&mut self, count: NonZeroU64, reason: PacketFailure) {
        let pending = self.pending.get_or_insert(PacketDropReport {
            dropped: 0,
            rejected: 0,
            last_failure: reason.clone(),
        });
        pending.dropped = pending.dropped.saturating_add(count.get());
        pending.last_failure = reason;
    }

    fn record_rejection(&mut self, reason: PacketFailure) {
        let pending = self.pending.get_or_insert(PacketDropReport {
            dropped: 0,
            rejected: 0,
            last_failure: reason.clone(),
        });
        pending.rejected = pending.rejected.saturating_add(1);
        pending.last_failure = reason;
    }
}

type MetadataDropSummary = DropSummary<MetadataDropReport>;

impl MetadataDropSummary {
    fn record_unmatched(&mut self, count: usize) {
        if count == 0 {
            return;
        }
        let pending = self
            .pending
            .get_or_insert(MetadataDropReport { unmatched: 0 });
        pending.unmatched = pending.unmatched.saturating_add(count);
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

    pub(crate) async fn metadata(&self, value: FrameMetadata) -> anyhow::Result<()> {
        self.tx
            .send(PipelineMessage::Metadata(value))
            .await
            .context("video pipeline stopped")
    }

    async fn unit(&self, value: Unit) -> anyhow::Result<()> {
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

    pub(crate) async fn receive(self, socket: UdpSocket) -> anyhow::Result<()> {
        let mut assembler = Assembler::default();
        let mut summary = PacketDropSummary::new();
        let mut buffer = vec![0; 65_536];
        loop {
            if let Some(report) = summary.report_if_due() {
                warn!(
                    dropped_packets = report.dropped,
                    rejected_packets = report.rejected,
                    reason = %report.last_failure,
                    "RTP packets dropped or rejected"
                );
            }
            let received = if let Some(deadline) = summary.deadline() {
                tokio::select! {
                    value = socket.recv_from(&mut buffer) => Some(value),
                    () = sleep_until(deadline) => None,
                }
            } else {
                Some(socket.recv_from(&mut buffer).await)
            };
            let Some(received) = received else {
                summary.force_due();
                continue;
            };
            let (length, source) = received?;
            if !source.ip().is_loopback() {
                summary.record_rejection(PacketFailure::Reject(PacketReject::NonLoopbackSource {
                    address: source,
                }));
                continue;
            }
            let packet = match decode_packet(&buffer[..length]) {
                Ok(packet) => packet,
                Err(error) => {
                    summary.record_rejection(PacketFailure::Reject(error));
                    continue;
                }
            };
            let result = assembler.consume(packet);
            if let Some(dropped) = result.drops {
                summary.record(dropped.packets, PacketFailure::Drop(dropped.reason));
            }
            match result.outcome {
                Ok(AssemblyOutcome::Complete(unit)) => self.unit(unit).await?,
                Ok(AssemblyOutcome::Dropped | AssemblyOutcome::Incomplete) => {}
                Err(error) => summary.record_rejection(PacketFailure::Reject(error)),
            }
        }
    }
}

impl VideoWorker {
    #[expect(
        clippy::too_many_lines,
        reason = "one worker loop keeps timer priority, resync recovery, and frame broadcasts ordered"
    )]
    pub(crate) async fn run(mut self) -> anyhow::Result<()> {
        let mut correlator = Correlator::new();
        let mut metadata_summary = MetadataDropSummary::new();
        let mut ready_generation = None;
        loop {
            let correlation = if correlator
                .deadline()
                .is_some_and(|deadline| deadline <= Instant::now())
            {
                // Once correlation work expires, do not poll ready input before resyncing it.
                correlator.deadline_expired()
            } else {
                if let Some(report) = metadata_summary.report_if_due() {
                    warn!(
                        unmatched_metadata = report.unmatched,
                        "frame metadata unmatched"
                    );
                }
                let timer = match (correlator.deadline(), metadata_summary.deadline()) {
                    (Some(correlation), Some(metadata)) if correlation <= metadata => {
                        Some((correlation, WorkerWake::CorrelatorDeadline))
                    }
                    (Some(_), Some(metadata)) => {
                        Some((metadata, WorkerWake::MetadataReportDeadline))
                    }
                    (Some(correlation), None) => {
                        Some((correlation, WorkerWake::CorrelatorDeadline))
                    }
                    (None, Some(metadata)) => Some((metadata, WorkerWake::MetadataReportDeadline)),
                    (None, None) => None,
                };
                let wake = if let Some((deadline, timer_wake)) = timer {
                    tokio::select! {
                        biased;
                        () = sleep_until(deadline) => timer_wake,
                        value = self.rx.recv() => match value {
                            Some(message) => WorkerWake::Message(message),
                            None => WorkerWake::PipelineClosed,
                        },
                    }
                } else {
                    match self.rx.recv().await {
                        Some(message) => WorkerWake::Message(message),
                        None => WorkerWake::PipelineClosed,
                    }
                };
                match wake {
                    WorkerWake::Message(PipelineMessage::Metadata(value)) => {
                        correlator.push_metadata(value)
                    }
                    WorkerWake::Message(PipelineMessage::Unit(value)) => {
                        correlator.push_unit(value)
                    }
                    WorkerWake::PipelineClosed => {
                        return Err(anyhow!("video pipeline closed"));
                    }
                    WorkerWake::CorrelatorDeadline => correlator.deadline_expired(),
                    WorkerWake::MetadataReportDeadline => {
                        metadata_summary.force_due();
                        continue;
                    }
                }
            };
            metadata_summary.record_unmatched(correlation.unmatched_metadata);

            let frames = match correlation.outcome {
                CorrelationOutcome::Frames(frames) => frames,
                CorrelationOutcome::Resync(reason) => {
                    warn!(reason = %reason, "video metadata/RTP correlation resynchronised");
                    self.readiness.await_keyframe();
                    if let Some(generation) = ready_generation.take()
                        && let Err(error) = self
                            .commands
                            .system(Command::KeyframeReadiness(KeyframeReadiness {
                                generation,
                                state: KeyframeState::Missing,
                            }))
                            .await
                    {
                        warn!(reason = %error, "keyframe readiness command failed");
                    }
                    continue;
                }
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
                if let Some((generation, state)) = transition
                    && let Err(error) = self
                        .commands
                        .system(Command::KeyframeReadiness(KeyframeReadiness {
                            generation,
                            state,
                        }))
                        .await
                {
                    warn!(reason = %error, "keyframe readiness command failed");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
