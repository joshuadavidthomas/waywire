use std::io;
use std::io::Write;
use std::ops::ControlFlow;
use std::os::fd::AsFd;
use std::os::unix::process::CommandExt;
use std::process::Child;
use std::process::ChildStdin;
use std::process::Command;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::Condvar;
use std::sync::Mutex;
use std::sync::PoisonError;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;
use std::time::Instant;

use calloop::ping::Ping;
use calloop::ping::PingSource;
use calloop::ping::make_ping;
use nix::errno::Errno;
use nix::fcntl::FcntlArg;
use nix::fcntl::OFlag;
use nix::fcntl::fcntl;
use nix::poll::PollFd;
use nix::poll::PollFlags;
use nix::poll::poll;
use nix::sys::signal::SigSet;
use nix::sys::signal::SigmaskHow;
use nix::sys::signal::pthread_sigmask;
use thiserror::Error;
use tracing::error;
use waywire_protocol::pipe::Chroma;
use waywire_protocol::pipe::Crf;
use waywire_protocol::pipe::Fps;
use waywire_protocol::pipe::FrameDimension;
use waywire_protocol::pipe::FrameMetadata;
use waywire_protocol::pipe::Generation;
use waywire_protocol::pipe::Kbps;
use waywire_protocol::pipe::MAX_RAW_PIXELS;
use waywire_protocol::pipe::ScalePercent;

const SLOT_COUNT: usize = 3;
// FFmpeg 8's SSRC option accepts only a signed integer. Stop at this boundary
// rather than changing the RTP identity or retrying an encoder that cannot start.
const MAX_MEDIA_GENERATION: u32 = i32::MAX as u32;
const CHILD_POLL_INTERVAL: Duration = Duration::from_millis(50);
const NOTIFICATION_STALL_LIMIT: Duration = Duration::from_secs(2);
const PIPE_WRITE_STALL_LIMIT: Duration = Duration::from_secs(5);
const PIPE_WRITE_POLL_INTERVAL_MS: u16 = 20;
const RESTART_DELAY_MIN: Duration = Duration::from_secs(1);
const RESTART_DELAY_MAX: Duration = Duration::from_secs(30);
const MAX_CONSECUTIVE_SPAWN_FAILURES: u8 = 6;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct EncoderConfig {
    pub(crate) raw_width: FrameDimension,
    pub(crate) raw_height: FrameDimension,
    pub(crate) encoded_width: FrameDimension,
    pub(crate) encoded_height: FrameDimension,
    pub(crate) fps: Fps,
    pub(crate) bitrate_kbps: Kbps,
    pub(crate) crf: Crf,
    pub(crate) chroma: Chroma,
}

impl EncoderConfig {
    pub(crate) fn validate(self) -> Result<usize, VideoError> {
        let pixels = u64::from(self.raw_width.get()) * u64::from(self.raw_height.get());
        if pixels > MAX_RAW_PIXELS {
            return Err(VideoError::InvalidFrame);
        }
        usize::try_from(pixels.checked_mul(4).ok_or(VideoError::InvalidFrame)?)
            .map_err(|_overflow| VideoError::InvalidFrame)
    }
}

#[derive(Debug)]
pub(crate) struct CapturedFrame<'a> {
    pixels: &'a [u8],
    metadata: FrameMetadata,
    config: EncoderConfig,
}

impl<'a> CapturedFrame<'a> {
    pub(crate) fn new(pixels: &'a [u8], metadata: FrameMetadata, config: EncoderConfig) -> Self {
        Self {
            pixels,
            metadata,
            config,
        }
    }

    fn validate(&self) -> Result<(), VideoError> {
        if self.pixels.len() != self.config.validate()?
            || self.metadata.generation.get() > MAX_MEDIA_GENERATION
            || self.metadata.width != self.config.encoded_width
            || self.metadata.height != self.config.encoded_height
            || self.metadata.fps != self.config.fps
            || self.metadata.chroma != self.config.chroma
        {
            return Err(VideoError::InvalidFrame);
        }
        Ok(())
    }
}

#[derive(Debug)]
struct RawFrame {
    pixels: Vec<u8>,
    metadata: FrameMetadata,
    config: EncoderConfig,
}

impl RawFrame {
    fn copy_from(storage: Vec<u8>, frame: CapturedFrame<'_>) -> Self {
        let mut pixels = storage;
        pixels.resize(frame.pixels.len(), 0);
        pixels.copy_from_slice(frame.pixels);
        Self {
            pixels,
            metadata: frame.metadata,
            config: frame.config,
        }
    }

    fn replace_from(&mut self, frame: CapturedFrame<'_>) {
        self.pixels.resize(frame.pixels.len(), 0);
        self.pixels.copy_from_slice(frame.pixels);
        self.metadata = frame.metadata;
        self.config = frame.config;
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Notification {
    Submitted(FrameMetadata),
    RestartRequired { generation: Generation },
    Fatal(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SubmitResult {
    Queued,
    ReplacedPending,
}

#[derive(Debug, Error)]
pub(crate) enum VideoError {
    #[error("raw frame or encoder configuration is invalid")]
    InvalidFrame,
    #[error("video encoder stopped")]
    Stopped,
    #[error("video frame pool exhausted")]
    PoolExhausted,
    #[error("could not start video worker: {0}")]
    SpawnThread(#[from] io::Error),
}

enum FrameSlot {
    Free(Vec<u8>),
    Pending(RawFrame),
    Encoding,
}

struct Pool {
    slots: [FrameSlot; SLOT_COUNT],
    pending: Option<usize>,
    encoding: Option<usize>,
    next_free: usize,
    generation: Generation,
    stopping: bool,
    notification_failure: Option<String>,
    child_pid: Option<u32>,
}

struct Shared {
    pool: Mutex<Pool>,
    work: Condvar,
}

pub(crate) struct VideoEncoder {
    shared: Arc<Shared>,
    notifications: mpsc::Receiver<Notification>,
    notification_source: Option<PingSource>,
    worker: Option<thread::JoinHandle<()>>,
}

impl VideoEncoder {
    pub(crate) fn start(ffmpeg: String, rtp_port: u16) -> Result<Self, VideoError> {
        Self::start_with_spawner(
            ffmpeg,
            rtp_port,
            RESTART_DELAY_MIN,
            RESTART_DELAY_MAX,
            spawn_ffmpeg,
        )
    }

    fn start_with_spawner<F>(
        ffmpeg: String,
        rtp_port: u16,
        restart_delay_min: Duration,
        restart_delay_max: Duration,
        spawn: F,
    ) -> Result<Self, VideoError>
    where
        F: FnMut(&str, u16, EncoderConfig, Generation) -> io::Result<EncoderProcess>
            + Send
            + 'static,
    {
        let initial_generation =
            Generation::new(1).map_err(|_invalid_generation| VideoError::InvalidFrame)?;
        let shared = Arc::new(Shared {
            pool: Mutex::new(Pool {
                slots: std::array::from_fn(|_| FrameSlot::Free(Vec::new())),
                pending: None,
                encoding: None,
                next_free: 0,
                generation: initial_generation,
                stopping: false,
                notification_failure: None,
                child_pid: None,
            }),
            work: Condvar::new(),
        });
        let (notifications_tx, notifications) = mpsc::sync_channel(64);
        let (notification_wake, notification_source) = make_ping()?;
        let worker_shared = Arc::clone(&shared);
        let worker = thread::Builder::new()
            .name("streamd-ffmpeg".into())
            .spawn(move || {
                Worker {
                    shared: &worker_shared,
                    notifications: &notifications_tx,
                    notification_wake: &notification_wake,
                    ffmpeg: &ffmpeg,
                    rtp_port,
                    process: None,
                    restart: RestartState::new(RestartBackoff {
                        min: restart_delay_min,
                        max: restart_delay_max,
                    }),
                    consecutive_spawn_failures: 0,
                }
                .run(spawn);
            })?;
        Ok(Self {
            shared,
            notifications,
            notification_source: Some(notification_source),
            worker: Some(worker),
        })
    }

    pub(crate) fn submit(&self, frame: CapturedFrame<'_>) -> Result<SubmitResult, VideoError> {
        frame.validate()?;
        let mut pool = self
            .shared
            .pool
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if pool.stopping {
            return Err(VideoError::Stopped);
        }
        if frame.metadata.generation != pool.generation {
            return Err(VideoError::InvalidFrame);
        }
        let result = if let Some(slot) = pool.pending
            && let FrameSlot::Pending(pending) = &mut pool.slots[slot]
        {
            pending.replace_from(frame);
            SubmitResult::ReplacedPending
        } else {
            let slot = (0..SLOT_COUNT)
                .map(|offset| (pool.next_free + offset) % SLOT_COUNT)
                .find(|index| matches!(pool.slots[*index], FrameSlot::Free(_)))
                .ok_or(VideoError::PoolExhausted)?;
            pool.next_free = (slot + 1) % SLOT_COUNT;
            let storage = match &mut pool.slots[slot] {
                FrameSlot::Free(storage) => std::mem::take(storage),
                FrameSlot::Pending(_) | FrameSlot::Encoding => {
                    return Err(VideoError::PoolExhausted);
                }
            };
            pool.slots[slot] = FrameSlot::Pending(RawFrame::copy_from(storage, frame));
            pool.pending = Some(slot);
            SubmitResult::Queued
        };
        self.shared.work.notify_one();
        Ok(result)
    }

    pub(crate) fn set_generation(&self, generation: Generation) -> Result<(), VideoError> {
        let mut pool = self
            .shared
            .pool
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if pool.stopping {
            return Err(VideoError::Stopped);
        }
        if generation.get() > MAX_MEDIA_GENERATION
            || pool.generation.get().checked_add(1) != Some(generation.get())
        {
            return Err(VideoError::InvalidFrame);
        }
        pool.generation = generation;
        if let Some(slot) = pool.pending {
            let discard = matches!(
                &pool.slots[slot],
                FrameSlot::Pending(frame) if frame.metadata.generation != generation
            );
            if discard {
                let salvaged =
                    match std::mem::replace(&mut pool.slots[slot], FrameSlot::Free(Vec::new())) {
                        FrameSlot::Pending(frame) => frame.pixels,
                        FrameSlot::Free(pixels) => pixels,
                        FrameSlot::Encoding => Vec::new(),
                    };
                pool.slots[slot] = FrameSlot::Free(salvaged);
                pool.pending = None;
            }
        }
        self.shared.work.notify_all();
        Ok(())
    }

    pub(crate) fn take_notification_source(&mut self) -> Option<PingSource> {
        self.notification_source.take()
    }

    pub(crate) fn try_notification(&self) -> Option<Notification> {
        match self.notifications.try_recv() {
            Ok(notification) => Some(notification),
            Err(mpsc::TryRecvError::Empty | mpsc::TryRecvError::Disconnected) => self
                .shared
                .pool
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .notification_failure
                .take()
                .map(Notification::Fatal),
        }
    }

    pub(crate) fn stop(mut self) {
        self.stop_inner();
    }

    fn stop_inner(&mut self) {
        {
            let mut pool = self
                .shared
                .pool
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            pool.stopping = true;
            self.shared.work.notify_all();
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }

    #[cfg(test)]
    fn child_pid(&self) -> Option<u32> {
        self.shared
            .pool
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .child_pid
    }
}

impl Drop for VideoEncoder {
    fn drop(&mut self) {
        self.stop_inner();
    }
}

struct EncoderProcess {
    child: Child,
    input: ChildStdin,
    config: EncoderConfig,
    generation: Generation,
}

#[derive(Clone, Copy)]
struct RestartBackoff {
    min: Duration,
    max: Duration,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EncoderTransition {
    Reuse,
    ReplaceForGeneration,
    RequestNewGeneration,
}

enum TransitionOutcome {
    Proceed(usize, RawFrame),
    Retry,
    Stop,
}

fn encoder_transition(
    active_config: EncoderConfig,
    active_generation: Generation,
    frame_config: EncoderConfig,
    frame_generation: Generation,
) -> EncoderTransition {
    if active_generation != frame_generation {
        EncoderTransition::ReplaceForGeneration
    } else if active_config != frame_config {
        EncoderTransition::RequestNewGeneration
    } else {
        EncoderTransition::Reuse
    }
}

impl EncoderProcess {
    fn stop(mut self) {
        drop(self.input);
        reap_child(&mut self.child);
    }
}

fn reap_child(child: &mut Child) {
    if !matches!(child.try_wait(), Ok(Some(_))) {
        let _ = child.kill();
        let _ = child.wait();
    }
}

struct RestartState {
    retry_at: Option<Instant>,
    delay: Duration,
    backoff: RestartBackoff,
}

impl RestartState {
    fn new(backoff: RestartBackoff) -> Self {
        Self {
            retry_at: None,
            delay: backoff.min,
            backoff,
        }
    }

    fn schedule(&mut self) {
        self.retry_at = Some(Instant::now() + self.delay);
        self.delay = (self.delay * 2).min(self.backoff.max);
    }

    fn reset(&mut self) {
        self.delay = self.backoff.min;
    }
}

fn child_has_exited(active: &mut EncoderProcess) -> bool {
    active.child.try_wait().ok().flatten().is_some()
}

struct Worker<'a> {
    shared: &'a Shared,
    notifications: &'a mpsc::SyncSender<Notification>,
    notification_wake: &'a Ping,
    ffmpeg: &'a str,
    rtp_port: u16,
    process: Option<EncoderProcess>,
    restart: RestartState,
    consecutive_spawn_failures: u8,
}

impl Worker<'_> {
    fn on_process_exit(&mut self, exited: EncoderProcess) -> ControlFlow<()> {
        let generation = exited.generation;
        stop_process(self.shared, Some(exited));
        if current_generation(self.shared) == generation {
            if !request_new_generation(
                self.shared,
                self.notifications,
                self.notification_wake,
                generation,
            ) {
                return ControlFlow::Break(());
            }
            self.restart.schedule();
        }
        ControlFlow::Continue(())
    }

    fn handle_spawn_failure(
        &mut self,
        slot: usize,
        frame: RawFrame,
        error: &io::Error,
    ) -> ControlFlow<()> {
        error!(%error, "could not start ffmpeg");
        self.consecutive_spawn_failures = self.consecutive_spawn_failures.saturating_add(1);
        if self.consecutive_spawn_failures >= MAX_CONSECUTIVE_SPAWN_FAILURES {
            release_slot(self.shared, slot, frame);
            let attempts = self.consecutive_spawn_failures;
            let _ = send_notification(
                self.shared,
                self.notifications,
                self.notification_wake,
                Notification::Fatal(format!(
                    "video encoder could not start after {attempts} attempts: {error}"
                )),
            );
            return ControlFlow::Break(());
        }
        self.restart.schedule();
        requeue_after_spawn_failure(self.shared, slot, frame);
        ControlFlow::Continue(())
    }

    fn apply_transition(&mut self, slot: usize, frame: RawFrame) -> TransitionOutcome {
        let transition = self
            .process
            .as_ref()
            .map_or(EncoderTransition::Reuse, |active| {
                encoder_transition(
                    active.config,
                    active.generation,
                    frame.config,
                    frame.metadata.generation,
                )
            });
        match transition {
            EncoderTransition::Reuse => TransitionOutcome::Proceed(slot, frame),
            EncoderTransition::ReplaceForGeneration => {
                stop_process(self.shared, self.process.take());
                TransitionOutcome::Proceed(slot, frame)
            }
            EncoderTransition::RequestNewGeneration => {
                let generation = frame.metadata.generation;
                stop_process(self.shared, self.process.take());
                release_slot(self.shared, slot, frame);
                if current_generation(self.shared) == generation
                    && !request_new_generation(
                        self.shared,
                        self.notifications,
                        self.notification_wake,
                        generation,
                    )
                {
                    return TransitionOutcome::Stop;
                }
                TransitionOutcome::Retry
            }
        }
    }

    fn handle_write_failure(
        &mut self,
        slot: usize,
        frame: RawFrame,
        error: &FrameWriteError,
    ) -> ControlFlow<()> {
        let frame_generation = frame.metadata.generation;
        match error {
            FrameWriteError::GenerationChanged { observed, frame } => error!(
                %error,
                observed_generation = ?observed,
                frame_generation = ?frame,
                "ffmpeg frame write aborted for a generation change"
            ),
            FrameWriteError::Stopping | FrameWriteError::Io(_) => {
                error!(%error, ?frame_generation, "ffmpeg frame write failed");
            }
        }
        stop_process(self.shared, self.process.take());
        release_slot(self.shared, slot, frame);
        if is_stopping(self.shared) {
            return ControlFlow::Break(());
        }
        if current_generation(self.shared) != frame_generation {
            return ControlFlow::Continue(());
        }
        if !request_new_generation(
            self.shared,
            self.notifications,
            self.notification_wake,
            frame_generation,
        ) {
            return ControlFlow::Break(());
        }
        self.restart.schedule();
        ControlFlow::Continue(())
    }

    fn run<F>(mut self, mut spawn: F)
    where
        F: FnMut(&str, u16, EncoderConfig, Generation) -> io::Result<EncoderProcess>,
    {
        loop {
            if is_stopping(self.shared) {
                stop_process(self.shared, self.process.take());
                return;
            }

            if self.process.as_mut().is_some_and(child_has_exited) {
                let Some(exited) = self.process.take() else {
                    continue;
                };
                if self.on_process_exit(exited).is_break() {
                    return;
                }
                continue;
            }

            if let Some(wait_until) = self.restart.retry_at.take()
                && !wait_until_or_stop(self.shared, wait_until)
            {
                stop_process(self.shared, self.process.take());
                return;
            }

            let Some((slot, frame)) = take_pending_frame(self.shared) else {
                continue;
            };

            if frame.metadata.generation != current_generation(self.shared) {
                release_slot(self.shared, slot, frame);
                continue;
            }

            if self.process.as_mut().is_some_and(child_has_exited) {
                let Some(exited) = self.process.take() else {
                    release_slot(self.shared, slot, frame);
                    continue;
                };
                release_slot(self.shared, slot, frame);
                if self.on_process_exit(exited).is_break() {
                    return;
                }
                continue;
            }

            let (slot, frame) = match self.apply_transition(slot, frame) {
                TransitionOutcome::Proceed(slot, frame) => (slot, frame),
                TransitionOutcome::Retry => continue,
                TransitionOutcome::Stop => return,
            };

            if self.process.is_none() {
                match spawn(
                    self.ffmpeg,
                    self.rtp_port,
                    frame.config,
                    frame.metadata.generation,
                ) {
                    Ok(started) => {
                        self.shared
                            .pool
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .child_pid = Some(started.child.id());
                        self.process = Some(started);
                        self.consecutive_spawn_failures = 0;
                    }
                    Err(error) => match self.handle_spawn_failure(slot, frame, &error) {
                        ControlFlow::Break(()) => return,
                        ControlFlow::Continue(()) => continue,
                    },
                }
            }

            let Some(active) = self.process.as_mut() else {
                release_slot(self.shared, slot, frame);
                continue;
            };
            let frame_generation = frame.metadata.generation;
            let write_result = write_frame(
                self.shared,
                &mut active.input,
                &frame.pixels,
                frame_generation,
            );
            if let Err(error) = write_result {
                match self.handle_write_failure(slot, frame, &error) {
                    ControlFlow::Break(()) => return,
                    ControlFlow::Continue(()) => continue,
                }
            }

            // Correlation metadata follows only a complete raw-frame pipe write.
            if !send_notification(
                self.shared,
                self.notifications,
                self.notification_wake,
                Notification::Submitted(frame.metadata.clone()),
            ) {
                stop_process(self.shared, self.process.take());
                release_slot(self.shared, slot, frame);
                return;
            }
            self.restart.reset();
            release_slot(self.shared, slot, frame);
        }
    }
}

fn take_pending_frame(shared: &Shared) -> Option<(usize, RawFrame)> {
    let mut pool = shared.pool.lock().unwrap_or_else(PoisonError::into_inner);
    if pool.pending.is_none() && !pool.stopping {
        pool = shared
            .work
            .wait_timeout(pool, CHILD_POLL_INTERVAL)
            .unwrap_or_else(PoisonError::into_inner)
            .0;
    }
    if pool.stopping {
        return None;
    }
    let slot = pool.pending.take()?;
    let FrameSlot::Pending(frame) = std::mem::replace(&mut pool.slots[slot], FrameSlot::Encoding)
    else {
        return None;
    };
    pool.encoding = Some(slot);
    Some((slot, frame))
}

fn request_new_generation(
    shared: &Shared,
    notifications: &mpsc::SyncSender<Notification>,
    notification_wake: &Ping,
    generation: Generation,
) -> bool {
    if !send_notification(
        shared,
        notifications,
        notification_wake,
        Notification::RestartRequired { generation },
    ) {
        return false;
    }
    let mut pool = shared.pool.lock().unwrap_or_else(PoisonError::into_inner);
    while pool.generation == generation && !pool.stopping {
        pool = shared
            .work
            .wait(pool)
            .unwrap_or_else(PoisonError::into_inner);
    }
    !pool.stopping
}

fn send_notification(
    shared: &Shared,
    notifications: &mpsc::SyncSender<Notification>,
    notification_wake: &Ping,
    notification: Notification,
) -> bool {
    send_notification_until(
        shared,
        notifications,
        notification_wake,
        notification,
        NOTIFICATION_STALL_LIMIT,
    )
}

fn send_notification_until(
    shared: &Shared,
    notifications: &mpsc::SyncSender<Notification>,
    notification_wake: &Ping,
    mut notification: Notification,
    stall_limit: Duration,
) -> bool {
    let started = Instant::now();
    loop {
        match notifications.try_send(notification) {
            Ok(()) => {
                notification_wake.ping();
                return true;
            }
            Err(mpsc::TrySendError::Disconnected(_)) => return false,
            Err(mpsc::TrySendError::Full(returned)) => {
                // A full queue still needs a wake: it may have filled after the
                // event loop drained an earlier, coalesced ping.
                notification_wake.ping();
                notification = returned;
                let mut pool = shared.pool.lock().unwrap_or_else(PoisonError::into_inner);
                if pool.stopping {
                    return false;
                }
                if started.elapsed() >= stall_limit {
                    pool.notification_failure =
                        Some("video notification queue remained full".into());
                    // Publish the side-channel fatal state before its wake so
                    // the event loop cannot clear readiness and miss it.
                    drop(pool);
                    notification_wake.ping();
                    return false;
                }
                drop(pool);
                thread::sleep(Duration::from_millis(5));
            }
        }
    }
}

fn wait_until_or_stop(shared: &Shared, deadline: Instant) -> bool {
    while Instant::now() < deadline {
        let pool = shared.pool.lock().unwrap_or_else(PoisonError::into_inner);
        if pool.stopping {
            return false;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        let pool = shared
            .work
            .wait_timeout(pool, remaining.min(Duration::from_millis(20)))
            .unwrap_or_else(PoisonError::into_inner)
            .0;
        if pool.stopping {
            return false;
        }
    }
    true
}

fn is_stopping(shared: &Shared) -> bool {
    shared
        .pool
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .stopping
}

fn current_generation(shared: &Shared) -> Generation {
    shared
        .pool
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .generation
}

fn release_slot(shared: &Shared, slot: usize, frame: RawFrame) {
    let mut pool = shared.pool.lock().unwrap_or_else(PoisonError::into_inner);
    assert_eq!(pool.encoding, Some(slot));
    assert!(matches!(pool.slots[slot], FrameSlot::Encoding));
    pool.encoding = None;
    pool.slots[slot] = FrameSlot::Free(frame.pixels);
    shared.work.notify_all();
}

fn requeue_after_spawn_failure(shared: &Shared, slot: usize, frame: RawFrame) {
    let mut pool = shared.pool.lock().unwrap_or_else(PoisonError::into_inner);
    assert_eq!(pool.encoding, Some(slot));
    assert!(matches!(pool.slots[slot], FrameSlot::Encoding));
    pool.encoding = None;

    // Any pending frame arrived after this worker took its frame. Keep it,
    // regardless of sequence wraparound; arrival owns the latest-frame policy.
    if pool.stopping || pool.generation != frame.metadata.generation || pool.pending.is_some() {
        pool.slots[slot] = FrameSlot::Free(frame.pixels);
    } else {
        pool.slots[slot] = FrameSlot::Pending(frame);
        pool.pending = Some(slot);
    }
    shared.work.notify_all();
}

fn stop_process(shared: &Shared, process: Option<EncoderProcess>) {
    shared
        .pool
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .child_pid = None;
    if let Some(process) = process {
        process.stop();
    }
}

fn spawn_ffmpeg(
    ffmpeg: &str,
    rtp_port: u16,
    config: EncoderConfig,
    generation: Generation,
) -> io::Result<EncoderProcess> {
    // The local RTP contract uses SSRC as the frame generation. The gateway
    // can correlate reordered replacement traffic by identity.
    let args = ffmpeg_args(rtp_port, config, generation);
    let mut command = Command::new(ffmpeg);
    command
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());
    reset_signal_mask_before_exec(&mut command);
    let mut child = command.spawn()?;
    let Some(input) = child.stdin.take() else {
        reap_child(&mut child);
        return Err(io::Error::other("ffmpeg stdin unavailable"));
    };
    if let Err(error) = fcntl(&input, FcntlArg::F_SETPIPE_SZ(1024 * 1024)) {
        drop(input);
        reap_child(&mut child);
        return Err(io::Error::other(error));
    }
    let flags = match fcntl(&input, FcntlArg::F_GETFL) {
        Ok(flags) => OFlag::from_bits_truncate(flags),
        Err(error) => {
            drop(input);
            reap_child(&mut child);
            return Err(io::Error::other(error));
        }
    };
    if let Err(error) = fcntl(&input, FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK)) {
        drop(input);
        reap_child(&mut child);
        return Err(io::Error::other(error));
    }
    Ok(EncoderProcess {
        child,
        input,
        config,
        generation,
    })
}

fn reset_signal_mask_before_exec(command: &mut Command) {
    // SAFETY: pthread_sigmask is async-signal-safe, and the closure only builds
    // an empty stack value and changes the calling thread's signal mask.
    unsafe {
        command.pre_exec(|| {
            pthread_sigmask(SigmaskHow::SIG_SETMASK, Some(&SigSet::empty()), None)
                .map_err(io::Error::from)
        });
    }
}

fn ffmpeg_args(rtp_port: u16, config: EncoderConfig, generation: Generation) -> Vec<String> {
    let rate = config.fps.get().to_string();
    let keyframe_interval = config.fps.keyframe_interval().to_string();
    let bitrate_ceiling = format!("{}k", config.bitrate_kbps.get());
    let buffer = format!("{}k", config.bitrate_kbps.get() * 2);
    let profile = config.chroma.h264_profile();
    vec![
        "-hide_banner".into(),
        "-loglevel".into(),
        "warning".into(),
        "-nostdin".into(),
        "-f".into(),
        "rawvideo".into(),
        "-pixel_format".into(),
        "bgra".into(),
        "-video_size".into(),
        format!("{}x{}", config.raw_width.get(), config.raw_height.get()),
        "-framerate".into(),
        rate.clone(),
        "-i".into(),
        "pipe:0".into(),
        "-an".into(),
        "-c:v".into(),
        "libx264".into(),
        "-preset".into(),
        "superfast".into(),
        "-tune".into(),
        "zerolatency".into(),
        "-profile:v".into(),
        profile.ffmpeg_profile().into(),
        "-level:v".into(),
        profile.ffmpeg_level(),
        "-pix_fmt".into(),
        config.chroma.ffmpeg_pixel_format().into(),
        "-crf".into(),
        config.crf.get().to_string(),
        "-maxrate".into(),
        bitrate_ceiling,
        "-bufsize".into(),
        buffer,
        "-g".into(),
        keyframe_interval.clone(),
        "-keyint_min".into(),
        keyframe_interval,
        "-sc_threshold".into(),
        "0".into(),
        "-bf".into(),
        "0".into(),
        "-x264-params".into(),
        // Write the desktop color contract into the H.264 VUI explicitly;
        // FFmpeg's frame metadata alone does not reach the SPS in every build.
        "aud=1:repeat-headers=1:colorprim=bt709:transfer=iec61966-2-1:colormatrix=bt709:fullrange=on".into(),
        "-vf".into(),
        // Preserve the desktop's full-range sRGB values through Chromium's
        // WebCodecs-to-canvas path. Chroma sampling comes from the quality ladder.
        format!(
            "scale={}:{}:out_color_matrix=bt709:out_range=pc",
            config.encoded_width.get(), config.encoded_height.get()
        ),
        "-colorspace".into(),
        "bt709".into(),
        "-color_primaries".into(),
        "bt709".into(),
        "-color_trc".into(),
        "iec61966-2-1".into(),
        "-color_range".into(),
        "pc".into(),
        "-payload_type".into(),
        "96".into(),
        "-ssrc".into(),
        generation.get().to_string(),
        "-rtpflags".into(),
        "skip_rtcp".into(),
        "-f".into(),
        "rtp".into(),
        format!("rtp://127.0.0.1:{rtp_port}?pkt_size=60000"),
    ]
}

#[derive(Debug, Error)]
enum FrameWriteError {
    #[error("encoder stopped during frame write")]
    Stopping,
    #[error("frame generation changed during write from {frame:?} to observed {observed:?}")]
    GenerationChanged {
        observed: Generation,
        frame: Generation,
    },
    #[error("ffmpeg pipe write failed")]
    Io(#[source] io::Error),
}

fn write_frame(
    shared: &Shared,
    output: &mut (impl Write + AsFd),
    bytes: &[u8],
    generation: Generation,
) -> Result<(), FrameWriteError> {
    let started = Instant::now();
    let mut written = 0;
    while written < bytes.len() {
        let pool = shared.pool.lock().unwrap_or_else(PoisonError::into_inner);
        if pool.stopping {
            return Err(FrameWriteError::Stopping);
        }
        if pool.generation != generation {
            return Err(FrameWriteError::GenerationChanged {
                observed: pool.generation,
                frame: generation,
            });
        }
        drop(pool);
        match output.write(&bytes[written..]) {
            Ok(0) => {
                return Err(FrameWriteError::Io(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "ffmpeg pipe closed",
                )));
            }
            Ok(count) => written += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                if started.elapsed() >= PIPE_WRITE_STALL_LIMIT {
                    return Err(FrameWriteError::Io(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "ffmpeg pipe stalled",
                    )));
                }
                let mut descriptors = [PollFd::new(output.as_fd(), PollFlags::POLLOUT)];
                match poll(&mut descriptors, PIPE_WRITE_POLL_INTERVAL_MS) {
                    Ok(_) | Err(Errno::EINTR) => {}
                    Err(error) => return Err(FrameWriteError::Io(io::Error::from(error))),
                }
            }
            Err(error) => return Err(FrameWriteError::Io(error)),
        }
    }
    Ok(())
}

pub(crate) fn encoded_dimensions(
    raw_width: u32,
    raw_height: u32,
    requested_scale: ScalePercent,
    fps: Fps,
) -> (u16, u16) {
    const MAX_FRAME_MACROBLOCKS: u32 = 36_864;
    const MAX_MACROBLOCKS_PER_SECOND: u32 = 2_073_600;
    let maximum = MAX_FRAME_MACROBLOCKS.min(MAX_MACROBLOCKS_PER_SECOND / fps.get());
    for scale in (1..=requested_scale.get()).rev() {
        let Some(scaled_width) = raw_width.checked_mul(scale) else {
            continue;
        };
        let Some(scaled_height) = raw_height.checked_mul(scale) else {
            continue;
        };
        let width = (scaled_width / 100).max(2) & !1;
        let height = (scaled_height / 100).max(2) & !1;
        let Some(macroblocks) = width.div_ceil(16).checked_mul(height.div_ceil(16)) else {
            continue;
        };
        if macroblocks <= maximum
            && let (Ok(width), Ok(height)) = (u16::try_from(width), u16::try_from(height))
        {
            return (width, height);
        }
    }
    (2, 2)
}

#[cfg(test)]
mod tests;
