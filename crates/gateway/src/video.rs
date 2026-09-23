mod assembler;
mod correlator;
mod hub;
mod readiness;
mod rtp;

use std::num::NonZeroU64;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use anyhow::anyhow;
use tokio::net::UdpSocket;
use tokio::sync::Semaphore;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio::time::sleep_until;
use tracing::warn;
#[cfg(test)]
use waywire_protocol::browser::Continuity;
#[cfg(test)]
use waywire_protocol::browser::FrameKind;
use waywire_protocol::browser::MAX_VIDEO_DATA_BYTES;
#[cfg(test)]
use waywire_protocol::browser::VideoSample;
#[cfg(test)]
use waywire_protocol::pipe::Chroma;
use waywire_protocol::pipe::Command;
#[cfg(test)]
use waywire_protocol::pipe::Fps;
use waywire_protocol::pipe::FrameMetadata;
#[cfg(test)]
use waywire_protocol::pipe::Generation;
use waywire_protocol::pipe::KeyframeReadiness;
use waywire_protocol::pipe::KeyframeState;

use self::assembler::Assembler;
#[cfg(test)]
use self::assembler::AssemblyDrop;
use self::assembler::AssemblyOutcome;
use self::assembler::PacketFailure;
use self::assembler::Unit;
use self::correlator::CorrelationOutcome;
use self::correlator::Correlator;
use self::correlator::MAX_PENDING_UNIT_BYTES;
use self::correlator::PENDING_RECORD_CAPACITY;
use self::correlator::PendingUnit;
pub(crate) use self::hub::VideoHub;
pub(crate) use self::readiness::Readiness;
#[cfg(test)]
pub(crate) use self::readiness::ReadinessState;
#[cfg(test)]
use self::rtp::AccessUnitEnd;
#[cfg(test)]
use self::rtp::Packet;
use self::rtp::PacketReject;
#[cfg(test)]
use self::rtp::RtpSequence;
#[cfg(test)]
use self::rtp::RtpTimestamp;
use self::rtp::decode_packet;
use crate::daemon::CommandSink;
use crate::session::Sessions;

const MAX_ACCESS_UNIT: usize = MAX_VIDEO_DATA_BYTES;
const UNMATCHED_DEADLINE: Duration = Duration::from_secs(2);
const DROP_REPORT_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum GopState {
    KeyframeCached,
    Recovering,
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
    sessions: Sessions,
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
        sessions: Sessions,
    ) -> (Self, VideoWorker) {
        let (tx, rx) = mpsc::channel(PENDING_RECORD_CAPACITY);
        let unit_budget = Arc::new(Semaphore::new(MAX_PENDING_UNIT_BYTES));
        (
            Self {
                tx,
                unit_budget,
                sessions,
            },
            VideoWorker {
                rx,
                hub,
                commands,
                readiness,
            },
        )
    }

    pub(crate) async fn metadata(&self, value: FrameMetadata) -> anyhow::Result<()> {
        // Observe ordered Submitted events before correlation can discard them.
        // Gaps here are producer replacement/backpressure, not RTP/browser loss.
        self.sessions.submitted(&value);
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
mod tests {
    use super::*;
    use crate::daemon::test_command_sink;

    pub(super) fn packet(
        sequence: u16,
        timestamp: u32,
        generation: u32,
        end: AccessUnitEnd,
        payload: &[u8],
    ) -> Packet {
        Packet {
            end,
            sequence: RtpSequence(sequence),
            timestamp: RtpTimestamp(timestamp),
            generation: Generation::new(generation).expect("test RTP generation should be valid"),
            payload: payload.to_vec(),
        }
    }

    pub(super) fn fps(value: u32) -> Fps {
        Fps::new(value).expect("test frame rate should be valid")
    }

    pub(super) fn video_hub() -> VideoHub {
        VideoHub::new(fps(60))
    }

    pub(super) fn metadata(sequence: u64, generation: u32) -> FrameMetadata {
        FrameMetadata {
            generation: Generation::new(generation)
                .expect("test metadata generation should be valid"),
            width: waywire_protocol::pipe::FrameDimension::new(1280)
                .expect("test frame width should be valid"),
            height: waywire_protocol::pipe::FrameDimension::new(720)
                .expect("test frame height should be valid"),
            capture_nanos: sequence,
            sequence,
            input_sequence: None,
            fps: fps(30),
            chroma: Chroma::Yuv444,
        }
    }

    pub(super) fn unit(timestamp: u32, generation: u32) -> Unit {
        Unit {
            data: vec![1].into(),
            kind: FrameKind::Key,
            continuity: Continuity::Continuous,
            timestamp: RtpTimestamp(timestamp),
            generation: Generation::new(generation).expect("test unit generation should be valid"),
        }
    }

    pub(super) fn consume(assembler: &mut Assembler, value: Packet) -> Option<Unit> {
        match assembler
            .consume(value)
            .outcome
            .expect("test RTP packet should assemble without a protocol error")
        {
            AssemblyOutcome::Complete(unit) => Some(unit),
            AssemblyOutcome::Incomplete | AssemblyOutcome::Dropped => None,
        }
    }

    pub(super) fn sample(sequence: u64, bytes: usize, kind: FrameKind) -> VideoSample {
        VideoSample {
            data: vec![1; bytes].into(),
            kind,
            continuity: Continuity::Continuous,
            metadata: metadata(sequence, 1),
        }
    }

    #[test]
    fn packet_drop_summary_keeps_packet_counts_and_the_last_reason() {
        let mut summary = PacketDropSummary::new();
        summary.record(
            NonZeroU64::new(3).expect("test count should be nonzero"),
            PacketFailure::Drop(AssemblyDrop::StaleGeneration),
        );
        summary.record_rejection(PacketFailure::Reject(PacketReject::InvalidHeader));

        let report = summary
            .report()
            .expect("recorded packet failures should report");
        assert_eq!(report.dropped, 3);
        assert_eq!(report.rejected, 1);
        assert_eq!(
            report.last_failure,
            PacketFailure::Reject(PacketReject::InvalidHeader)
        );
        assert!(summary.deadline().is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn metadata_drop_summary_reports_a_later_drop_while_idle() {
        let mut summary = MetadataDropSummary::new();
        summary.record_unmatched(1);
        let deadline = summary
            .deadline()
            .expect("pending metadata summary should have a deadline");
        sleep_until(deadline).await;
        let first = summary
            .report_if_due()
            .expect("initial unmatched metadata should become due");
        assert_eq!(first.unmatched, 1);

        summary.record_unmatched(2);
        let deadline = summary
            .deadline()
            .expect("pending metadata summary should have a deadline");
        sleep_until(deadline).await;
        let second = summary
            .report_if_due()
            .expect("metadata summary should become due without another record");
        assert_eq!(second.unmatched, 2);
    }

    #[tokio::test]
    async fn submitted_gaps_adapt_only_for_the_owner_before_rtp_correlation() {
        use waywire_protocol::browser::Feedback;
        use waywire_protocol::browser::FeedbackValues;
        use waywire_protocol::pipe::Kbps;
        use waywire_protocol::pipe::ScalePercent;

        use crate::session::FeedbackOutcome;
        use crate::session::SocketId;

        for step in [1, 2] {
            let (commands, mut command_events) = test_command_sink();
            let sessions = Sessions::new(
                commands.clone(),
                Kbps::new(8_000).expect("bitrate"),
                fps(60),
            );
            let owner = SocketId::new(1);
            sessions.acquire(owner).await.expect("acquire");
            assert!(matches!(
                command_events.recv().await,
                Some(Command::ReleaseAll(_))
            ));
            let (pipeline, mut worker) =
                VideoPipeline::new(video_hub(), commands, Readiness::new(), sessions.clone());
            // Drain metadata without ever correlating RTP: producer pressure must
            // not depend on successful encoded delivery or correlate-time drops.
            let drain = tokio::spawn(async move { while worker.rx.recv().await.is_some() {} });
            let feedback = Feedback::new(FeedbackValues {
                received: 30,
                presented: 30,
                dropped: 0,
                queue_peak: 0,
                queue_busy_ms: 0.0,
                sample_ms: 1000.0,
                rtt: 20.0,
            })
            .expect("healthy browser feedback");
            let mut frame = metadata(1, 1);
            frame.chroma = Chroma::Yuv420;
            pipeline.metadata(frame.clone()).await.expect("baseline");
            frame.sequence += 1;
            frame.capture_nanos = 2_000_000_000;
            pipeline.metadata(frame.clone()).await.expect("warmup");
            for _ in 0..2 {
                for _ in 0..30 {
                    frame.sequence += step;
                    frame.capture_nanos += 33_000_000;
                    pipeline.metadata(frame.clone()).await.expect("submitted");
                }
                assert_eq!(
                    sessions
                        .feedback(SocketId::new(2), feedback)
                        .await
                        .expect("non-owner"),
                    FeedbackOutcome::NotInputOwner
                );
                sessions
                    .feedback(owner, feedback)
                    .await
                    .expect("owner feedback");
            }
            assert_eq!(sessions.quality_chroma(), Chroma::Yuv420);
            assert_eq!(sessions.quality().fps.get(), 60);
            assert_eq!(
                sessions.quality().scale.get(),
                if step == 1 { 100 } else { 75 }
            );
            if step == 2 {
                assert_eq!(
                    command_events.recv().await,
                    Some(Command::Quality(waywire_protocol::pipe::Quality {
                        bitrate_kbps: Kbps::new(8_000).expect("bitrate"),
                        fps: fps(60),
                        scale_percent: ScalePercent::new(75).expect("scale"),
                        crf: waywire_protocol::pipe::Crf::new(23).expect("crf"),
                        chroma: Chroma::Yuv420,
                    }))
                );
            }
            drop(pipeline);
            drain.await.expect("metadata drained");
        }
    }

    #[tokio::test]
    async fn worker_resync_waits_for_a_fresh_keyframe_before_resuming_delivery() {
        let (commands, mut command_events) = test_command_sink();
        let readiness = Readiness::new();
        let hub = video_hub();
        let sessions = Sessions::new(
            commands.clone(),
            waywire_protocol::pipe::Kbps::new(8_000).expect("valid bitrate"),
            Fps::new(60).expect("valid fps"),
        );
        let (pipeline, worker) =
            VideoPipeline::new(hub.clone(), commands, readiness.clone(), sessions);
        let worker_task = tokio::spawn(worker.run());

        pipeline
            .metadata(metadata(1, 1))
            .await
            .expect("first metadata should queue");
        pipeline
            .unit(unit(1, 1))
            .await
            .expect("first keyframe should queue");
        assert!(matches!(
            command_events.recv().await,
            Some(Command::KeyframeReadiness(KeyframeReadiness {
                state: KeyframeState::Cached,
                ..
            }))
        ));
        assert_eq!(readiness.state(), ReadinessState::Ready);
        let mut viewer = hub.subscribe();
        assert_eq!(viewer.next().await.metadata.sequence, 1);

        for sequence in 2..=(PENDING_RECORD_CAPACITY as u64 + 2) {
            pipeline
                .metadata(metadata(sequence, 1))
                .await
                .expect("backlog metadata should queue");
        }
        assert!(matches!(
            command_events.recv().await,
            Some(Command::KeyframeReadiness(KeyframeReadiness {
                state: KeyframeState::Missing,
                ..
            }))
        ));
        assert_eq!(readiness.state(), ReadinessState::WaitingForKeyframe);

        pipeline
            .metadata(metadata(500, 1))
            .await
            .expect("recovery delta metadata should queue");
        let mut delta = unit(2, 1);
        delta.kind = FrameKind::Delta;
        pipeline
            .unit(delta)
            .await
            .expect("recovery delta should queue");
        assert!(
            tokio::time::timeout(Duration::from_millis(20), command_events.recv())
                .await
                .is_err()
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(20), viewer.next())
                .await
                .is_err()
        );

        pipeline
            .metadata(metadata(501, 1))
            .await
            .expect("recovery keyframe metadata should queue");
        pipeline
            .unit(unit(3, 1))
            .await
            .expect("recovery keyframe should queue");
        assert!(matches!(
            command_events.recv().await,
            Some(Command::KeyframeReadiness(KeyframeReadiness {
                state: KeyframeState::Cached,
                ..
            }))
        ));
        assert_eq!(readiness.state(), ReadinessState::Ready);
        let frame = viewer.next().await;
        assert_eq!(frame.metadata.sequence, 501);
        assert_eq!(frame.kind, FrameKind::Key);
        assert_eq!(frame.continuity, Continuity::AfterGap);

        drop(pipeline);
        assert!(worker_task.await.expect("worker task should run").is_err());
    }
}
