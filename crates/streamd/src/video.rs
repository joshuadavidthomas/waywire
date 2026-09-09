use std::{
    io::{self, Write},
    os::{fd::AsFd, unix::process::CommandExt},
    process::{Child, ChildStdin, Command, Stdio},
    sync::{Arc, Condvar, Mutex, mpsc},
    thread,
    time::{Duration, Instant},
};

use calloop::ping::{Ping, PingSource, make_ping};
use nix::{
    errno::Errno,
    fcntl::{FcntlArg, OFlag, fcntl},
    poll::{PollFd, PollFlags, poll},
    sys::signal::{SigSet, SigmaskHow, pthread_sigmask},
};
use thiserror::Error;

use crate::protocol::{FrameMetadata, MAX_RAW_PIXELS};

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
pub struct EncoderConfig {
    pub raw_width: u32,
    pub raw_height: u32,
    pub encoded_width: u16,
    pub encoded_height: u16,
    pub fps: u32,
    pub bitrate_kbps: u32,
}

impl EncoderConfig {
    pub fn validate(self) -> Result<usize, VideoError> {
        let pixels = u64::from(self.raw_width) * u64::from(self.raw_height);
        if self.raw_width == 0
            || self.raw_height == 0
            || pixels > MAX_RAW_PIXELS
            || self.encoded_width == 0
            || self.encoded_height == 0
            || !(10..=120).contains(&self.fps)
            || !(300..=50_000).contains(&self.bitrate_kbps)
        {
            return Err(VideoError::InvalidFrame);
        }
        usize::try_from(pixels.checked_mul(4).ok_or(VideoError::InvalidFrame)?)
            .map_err(|_| VideoError::InvalidFrame)
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
            || self.metadata.generation == 0
            || self.metadata.generation > MAX_MEDIA_GENERATION
            || self.metadata.width != self.config.encoded_width
            || self.metadata.height != self.config.encoded_height
            || self.metadata.fps != self.config.fps
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
pub enum Notification {
    Submitted(FrameMetadata),
    RestartRequired { generation: u32 },
    Fatal(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubmitResult {
    Queued,
    ReplacedPending,
}

#[derive(Debug, Error)]
pub enum VideoError {
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
    generation: u32,
    stopping: bool,
    notification_failure: Option<String>,
    child_pid: Option<u32>,
}

struct Shared {
    pool: Mutex<Pool>,
    work: Condvar,
}

pub struct VideoEncoder {
    shared: Arc<Shared>,
    notifications: mpsc::Receiver<Notification>,
    notification_source: Option<PingSource>,
    worker: Option<thread::JoinHandle<()>>,
}

impl VideoEncoder {
    pub fn start(ffmpeg: String, rtp_port: u16) -> Result<Self, VideoError> {
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
        F: FnMut(&str, u16, EncoderConfig, u32) -> io::Result<EncoderProcess> + Send + 'static,
    {
        let shared = Arc::new(Shared {
            pool: Mutex::new(Pool {
                slots: std::array::from_fn(|_| FrameSlot::Free(Vec::new())),
                pending: None,
                encoding: None,
                next_free: 0,
                generation: 1,
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
                worker_main(
                    worker_shared,
                    notifications_tx,
                    notification_wake,
                    ffmpeg,
                    rtp_port,
                    RestartBackoff {
                        min: restart_delay_min,
                        max: restart_delay_max,
                    },
                    spawn,
                )
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
        let mut pool = self.shared.pool.lock().unwrap();
        if pool.stopping {
            return Err(VideoError::Stopped);
        }
        if frame.metadata.generation != pool.generation {
            return Err(VideoError::InvalidFrame);
        }
        let result = if let Some(slot) = pool.pending {
            let FrameSlot::Pending(pending) = &mut pool.slots[slot] else {
                unreachable!("pending slot state disagrees with pool index");
            };
            pending.replace_from(frame);
            SubmitResult::ReplacedPending
        } else {
            let slot = (0..SLOT_COUNT)
                .map(|offset| (pool.next_free + offset) % SLOT_COUNT)
                .find(|index| matches!(pool.slots[*index], FrameSlot::Free(_)))
                .ok_or(VideoError::PoolExhausted)?;
            pool.next_free = (slot + 1) % SLOT_COUNT;
            let FrameSlot::Free(storage) =
                std::mem::replace(&mut pool.slots[slot], FrameSlot::Free(Vec::new()))
            else {
                unreachable!("selected frame slot is not free");
            };
            pool.slots[slot] = FrameSlot::Pending(RawFrame::copy_from(storage, frame));
            pool.pending = Some(slot);
            SubmitResult::Queued
        };
        self.shared.work.notify_one();
        Ok(result)
    }

    pub fn set_generation(&self, generation: u32) -> Result<(), VideoError> {
        let mut pool = self.shared.pool.lock().unwrap();
        if pool.stopping {
            return Err(VideoError::Stopped);
        }
        if generation > MAX_MEDIA_GENERATION || pool.generation.checked_add(1) != Some(generation) {
            return Err(VideoError::InvalidFrame);
        }
        pool.generation = generation;
        if let Some(slot) = pool.pending {
            let discard = matches!(
                &pool.slots[slot],
                FrameSlot::Pending(frame) if frame.metadata.generation != generation
            );
            if discard {
                let FrameSlot::Pending(frame) =
                    std::mem::replace(&mut pool.slots[slot], FrameSlot::Free(Vec::new()))
                else {
                    unreachable!("pending slot state disagrees with pool index");
                };
                pool.slots[slot] = FrameSlot::Free(frame.pixels);
                pool.pending = None;
            }
        }
        self.shared.work.notify_all();
        Ok(())
    }

    pub(crate) fn take_notification_source(&mut self) -> Option<PingSource> {
        self.notification_source.take()
    }

    pub fn try_notification(&self) -> Option<Notification> {
        match self.notifications.try_recv() {
            Ok(notification) => Some(notification),
            Err(mpsc::TryRecvError::Empty | mpsc::TryRecvError::Disconnected) => self
                .shared
                .pool
                .lock()
                .unwrap()
                .notification_failure
                .take()
                .map(Notification::Fatal),
        }
    }

    pub fn stop(mut self) {
        self.stop_inner();
    }

    fn stop_inner(&mut self) {
        {
            let mut pool = self.shared.pool.lock().unwrap();
            pool.stopping = true;
            self.shared.work.notify_all();
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }

    #[cfg(test)]
    fn child_pid(&self) -> Option<u32> {
        self.shared.pool.lock().unwrap().child_pid
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
    generation: u32,
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

fn encoder_transition(
    active_config: EncoderConfig,
    active_generation: u32,
    frame_config: EncoderConfig,
    frame_generation: u32,
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
    match child.try_wait() {
        Ok(Some(_)) => {}
        Ok(None) | Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

fn worker_main<F>(
    shared: Arc<Shared>,
    notifications: mpsc::SyncSender<Notification>,
    notification_wake: Ping,
    ffmpeg: String,
    rtp_port: u16,
    restart_backoff: RestartBackoff,
    mut spawn: F,
) where
    F: FnMut(&str, u16, EncoderConfig, u32) -> io::Result<EncoderProcess>,
{
    let mut process: Option<EncoderProcess> = None;
    let mut restart_delay = restart_backoff.min;
    let mut retry_at: Option<Instant> = None;
    let mut consecutive_spawn_failures = 0_u8;

    loop {
        if is_stopping(&shared) {
            stop_process(&shared, process.take());
            return;
        }

        if process
            .as_mut()
            .is_some_and(|active| active.child.try_wait().ok().flatten().is_some())
        {
            let exited = process.take().unwrap();
            let generation = exited.generation;
            stop_process(&shared, Some(exited));
            if current_generation(&shared) == generation {
                if !request_new_generation(&shared, &notifications, &notification_wake, generation)
                {
                    return;
                }
                retry_at = Some(Instant::now() + restart_delay);
                restart_delay = (restart_delay * 2).min(restart_backoff.max);
            }
            continue;
        }

        if let Some(wait_until) = retry_at.take()
            && !wait_until_or_stop(&shared, wait_until)
        {
            stop_process(&shared, process.take());
            return;
        }

        let Some((slot, frame)) = take_pending_frame(&shared) else {
            continue;
        };

        if frame.metadata.generation != current_generation(&shared) {
            release_slot(&shared, slot, frame);
            continue;
        }

        let child_exited = process
            .as_mut()
            .is_some_and(|active| active.child.try_wait().ok().flatten().is_some());
        if child_exited {
            let exited = process.take().unwrap();
            let generation = exited.generation;
            stop_process(&shared, Some(exited));
            release_slot(&shared, slot, frame);
            if current_generation(&shared) == generation {
                if !request_new_generation(&shared, &notifications, &notification_wake, generation)
                {
                    return;
                }
                retry_at = Some(Instant::now() + restart_delay);
                restart_delay = (restart_delay * 2).min(restart_backoff.max);
            }
            continue;
        }

        let transition = process.as_ref().map_or(EncoderTransition::Reuse, |active| {
            encoder_transition(
                active.config,
                active.generation,
                frame.config,
                frame.metadata.generation,
            )
        });
        match transition {
            EncoderTransition::Reuse => {}
            EncoderTransition::ReplaceForGeneration => {
                stop_process(&shared, process.take());
            }
            EncoderTransition::RequestNewGeneration => {
                let generation = frame.metadata.generation;
                stop_process(&shared, process.take());
                release_slot(&shared, slot, frame);
                if current_generation(&shared) == generation
                    && !request_new_generation(
                        &shared,
                        &notifications,
                        &notification_wake,
                        generation,
                    )
                {
                    return;
                }
                continue;
            }
        }

        if process.is_none() {
            match spawn(&ffmpeg, rtp_port, frame.config, frame.metadata.generation) {
                Ok(started) => {
                    shared.pool.lock().unwrap().child_pid = Some(started.child.id());
                    process = Some(started);
                    consecutive_spawn_failures = 0;
                }
                Err(error) => {
                    eprintln!("sprite-desktop-streamd: could not start ffmpeg: {error}");
                    consecutive_spawn_failures = consecutive_spawn_failures.saturating_add(1);
                    if consecutive_spawn_failures >= MAX_CONSECUTIVE_SPAWN_FAILURES {
                        release_slot(&shared, slot, frame);
                        let _ = send_notification(
                            &shared,
                            &notifications,
                            &notification_wake,
                            Notification::Fatal(format!(
                                "video encoder could not start after {consecutive_spawn_failures} attempts: {error}"
                            )),
                        );
                        return;
                    }
                    retry_at = Some(Instant::now() + restart_delay);
                    restart_delay = (restart_delay * 2).min(restart_backoff.max);
                    requeue_after_spawn_failure(&shared, slot, frame);
                    continue;
                }
            }
        }

        let active = process.as_mut().unwrap();
        let frame_generation = frame.metadata.generation;
        if let Err(error) = write_frame(&shared, &mut active.input, &frame.pixels, frame_generation)
        {
            let generation = active.generation;
            eprintln!("sprite-desktop-streamd: ffmpeg frame write failed: {error}");
            stop_process(&shared, process.take());
            release_slot(&shared, slot, frame);
            if is_stopping(&shared) {
                return;
            }
            if current_generation(&shared) != frame_generation {
                continue;
            }
            if !request_new_generation(&shared, &notifications, &notification_wake, generation) {
                return;
            }
            retry_at = Some(Instant::now() + restart_delay);
            restart_delay = (restart_delay * 2).min(restart_backoff.max);
            continue;
        }

        // Correlation metadata follows only a complete raw-frame pipe write.
        if !send_notification(
            &shared,
            &notifications,
            &notification_wake,
            Notification::Submitted(frame.metadata.clone()),
        ) {
            stop_process(&shared, process.take());
            release_slot(&shared, slot, frame);
            return;
        }
        restart_delay = restart_backoff.min;
        release_slot(&shared, slot, frame);
    }
}

fn take_pending_frame(shared: &Shared) -> Option<(usize, RawFrame)> {
    let mut pool = shared.pool.lock().unwrap();
    if pool.pending.is_none() && !pool.stopping {
        let (next, _) = shared.work.wait_timeout(pool, CHILD_POLL_INTERVAL).unwrap();
        pool = next;
    }
    if pool.stopping || pool.pending.is_none() {
        return None;
    }
    let slot = pool.pending.take().unwrap();
    let FrameSlot::Pending(frame) = std::mem::replace(&mut pool.slots[slot], FrameSlot::Encoding)
    else {
        unreachable!("pending slot state disagrees with pool index");
    };
    pool.encoding = Some(slot);
    Some((slot, frame))
}

fn request_new_generation(
    shared: &Shared,
    notifications: &mpsc::SyncSender<Notification>,
    notification_wake: &Ping,
    generation: u32,
) -> bool {
    if !send_notification(
        shared,
        notifications,
        notification_wake,
        Notification::RestartRequired { generation },
    ) {
        return false;
    }
    let mut pool = shared.pool.lock().unwrap();
    while pool.generation == generation && !pool.stopping {
        pool = shared.work.wait(pool).unwrap();
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
                let mut pool = shared.pool.lock().unwrap();
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
        let pool = shared.pool.lock().unwrap();
        if pool.stopping {
            return false;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        let (pool, _) = shared
            .work
            .wait_timeout(pool, remaining.min(Duration::from_millis(20)))
            .unwrap();
        if pool.stopping {
            return false;
        }
    }
    true
}

fn is_stopping(shared: &Shared) -> bool {
    shared.pool.lock().unwrap().stopping
}

fn current_generation(shared: &Shared) -> u32 {
    shared.pool.lock().unwrap().generation
}

fn release_slot(shared: &Shared, slot: usize, frame: RawFrame) {
    let mut pool = shared.pool.lock().unwrap();
    assert_eq!(pool.encoding, Some(slot));
    assert!(matches!(pool.slots[slot], FrameSlot::Encoding));
    pool.encoding = None;
    pool.slots[slot] = FrameSlot::Free(frame.pixels);
    shared.work.notify_all();
}

fn requeue_after_spawn_failure(shared: &Shared, slot: usize, frame: RawFrame) {
    let mut pool = shared.pool.lock().unwrap();
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
    shared.pool.lock().unwrap().child_pid = None;
    if let Some(process) = process {
        process.stop();
    }
}

fn spawn_ffmpeg(
    ffmpeg: &str,
    rtp_port: u16,
    config: EncoderConfig,
    generation: u32,
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
    let input = match child.stdin.take() {
        Some(input) => input,
        None => {
            reap_child(&mut child);
            return Err(io::Error::other("ffmpeg stdin unavailable"));
        }
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

fn ffmpeg_args(rtp_port: u16, config: EncoderConfig, generation: u32) -> Vec<String> {
    let rate = config.fps.to_string();
    let keyframe_interval = config.fps.div_ceil(4).to_string();
    let bitrate = format!("{}k", config.bitrate_kbps);
    let peak = format!("{}k", config.bitrate_kbps * 2);
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
        format!("{}x{}", config.raw_width, config.raw_height),
        "-framerate".into(),
        rate.clone(),
        "-re".into(),
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
        "high444".into(),
        "-level:v".into(),
        "5.2".into(),
        "-pix_fmt".into(),
        "yuv444p".into(),
        "-b:v".into(),
        bitrate,
        "-maxrate".into(),
        peak.clone(),
        "-bufsize".into(),
        peak,
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
        // Preserve the desktop's full-range sRGB values and 4:4:4 chroma
        // through Chromium's WebCodecs-to-canvas path.
        format!(
            "scale={}:{}:out_color_matrix=bt709:out_range=pc",
            config.encoded_width, config.encoded_height
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
        generation.to_string(),
        "-rtpflags".into(),
        "skip_rtcp".into(),
        "-f".into(),
        "rtp".into(),
        format!("rtp://127.0.0.1:{rtp_port}?pkt_size=60000"),
    ]
}

fn write_frame(
    shared: &Shared,
    output: &mut (impl Write + AsFd),
    bytes: &[u8],
    generation: u32,
) -> io::Result<()> {
    let started = Instant::now();
    let mut written = 0;
    while written < bytes.len() {
        let pool = shared.pool.lock().unwrap();
        if pool.stopping {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "encoder stopping",
            ));
        }
        if pool.generation != generation {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "frame generation changed during write",
            ));
        }
        drop(pool);
        match output.write(&bytes[written..]) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "ffmpeg pipe closed",
                ));
            }
            Ok(count) => written += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                if started.elapsed() >= PIPE_WRITE_STALL_LIMIT {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "ffmpeg pipe stalled",
                    ));
                }
                let mut descriptors = [PollFd::new(output.as_fd(), PollFlags::POLLOUT)];
                match poll(&mut descriptors, PIPE_WRITE_POLL_INTERVAL_MS) {
                    Ok(_) | Err(Errno::EINTR) => {}
                    Err(error) => return Err(io::Error::from(error)),
                }
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

pub fn encoded_dimensions(
    raw_width: u32,
    raw_height: u32,
    requested_scale: u32,
    fps: u32,
) -> (u16, u16) {
    const MAX_FRAME_MACROBLOCKS: u32 = 36_864;
    const MAX_MACROBLOCKS_PER_SECOND: u32 = 2_073_600;
    let maximum = MAX_FRAME_MACROBLOCKS.min(MAX_MACROBLOCKS_PER_SECOND / fps);
    for scale in (1..=requested_scale).rev() {
        let width = (raw_width * scale / 100).max(2) & !1;
        let height = (raw_height * scale / 100).max(2) & !1;
        let macroblocks = width.div_ceil(16) * height.div_ceil(16);
        if macroblocks <= maximum {
            return (width as u16, height as u16);
        }
    }
    (2, 2)
}

#[cfg(test)]
mod tests;
