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
mod tests {
    use std::{
        fs::File,
        io::Read,
        net::UdpSocket,
        os::unix::process::ExitStatusExt,
        sync::atomic::{AtomicUsize, Ordering},
    };

    use calloop::EventLoop;
    use nix::{
        fcntl::{FcntlArg, OFlag, fcntl},
        sys::signal::{Signal, kill},
        unistd::{Pid, pipe},
    };

    use super::*;

    fn frame(sequence: u64, generation: u32) -> RawFrame {
        RawFrame {
            pixels: vec![sequence as u8; 16],
            metadata: FrameMetadata {
                generation,
                width: 2,
                height: 2,
                capture_nanos: sequence,
                sequence,
                input_sequence: 0,
                fps: 60,
            },
            config: EncoderConfig {
                raw_width: 2,
                raw_height: 2,
                encoded_width: 2,
                encoded_height: 2,
                fps: 60,
                bitrate_kbps: 8_000,
            },
        }
    }

    fn captured(frame: &RawFrame) -> CapturedFrame<'_> {
        CapturedFrame::new(&frame.pixels, frame.metadata.clone(), frame.config)
    }

    fn submit_frame(encoder: &VideoEncoder, frame: &RawFrame) -> Result<SubmitResult, VideoError> {
        encoder.submit(captured(frame))
    }

    fn sized_frame(sequence: u64, generation: u32, width: u32, height: u32) -> RawFrame {
        let config = EncoderConfig {
            raw_width: width,
            raw_height: height,
            encoded_width: u16::try_from(width).unwrap(),
            encoded_height: u16::try_from(height).unwrap(),
            fps: 60,
            bitrate_kbps: 8_000,
        };
        RawFrame {
            pixels: vec![sequence as u8; config.validate().unwrap()],
            metadata: FrameMetadata {
                generation,
                width: config.encoded_width,
                height: config.encoded_height,
                capture_nanos: sequence,
                sequence,
                input_sequence: u32::try_from(sequence).unwrap(),
                fps: config.fps,
            },
            config,
        }
    }

    fn real_frame(sequence: u64, generation: u32) -> RawFrame {
        real_frame_at_fps(sequence, generation, 60)
    }

    fn real_frame_at_fps(sequence: u64, generation: u32, fps: u32) -> RawFrame {
        let config = EncoderConfig {
            raw_width: 320,
            raw_height: 180,
            encoded_width: 320,
            encoded_height: 180,
            fps,
            bitrate_kbps: 8_000,
        };
        RawFrame {
            pixels: vec![sequence as u8; config.validate().unwrap()],
            metadata: FrameMetadata {
                generation,
                width: config.encoded_width,
                height: config.encoded_height,
                capture_nanos: sequence,
                sequence,
                input_sequence: 0,
                fps: config.fps,
            },
            config,
        }
    }

    fn shared() -> Arc<Shared> {
        Arc::new(Shared {
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
        })
    }

    fn nonblocking_pipe() -> (File, File, usize) {
        let (reader, writer) = pipe().unwrap();
        let requested_capacity = 64 * 1024;
        let _ = fcntl(&writer, FcntlArg::F_SETPIPE_SZ(requested_capacity));
        let capacity = usize::try_from(fcntl(&writer, FcntlArg::F_GETPIPE_SZ).unwrap()).unwrap();
        let flags = OFlag::from_bits_truncate(fcntl(&writer, FcntlArg::F_GETFL).unwrap());
        fcntl(&writer, FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK)).unwrap();
        (File::from(reader), File::from(writer), capacity)
    }

    fn wait_for_notification(encoder: &VideoEncoder) -> Notification {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(notification) = encoder.try_notification() {
                return notification;
            }
            assert!(Instant::now() < deadline, "encoder notification timed out");
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn spawn_test_sink(config: EncoderConfig, generation: u32) -> io::Result<EncoderProcess> {
        let mut command = Command::new("cat");
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        reset_signal_mask_before_exec(&mut command);
        let mut child = command.spawn()?;
        let input = child
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("test sink stdin unavailable"))?;
        let flags =
            OFlag::from_bits_truncate(fcntl(&input, FcntlArg::F_GETFL).map_err(io::Error::other)?);
        fcntl(&input, FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK)).map_err(io::Error::other)?;
        Ok(EncoderProcess {
            child,
            input,
            config,
            generation,
        })
    }

    fn encoder_with_spawner<F>(restart_delay: Duration, spawn: F) -> VideoEncoder
    where
        F: FnMut(&str, u16, EncoderConfig, u32) -> io::Result<EncoderProcess> + Send + 'static,
    {
        VideoEncoder::start_with_spawner(
            "test-encoder".into(),
            0,
            restart_delay,
            restart_delay,
            spawn,
        )
        .unwrap()
    }

    fn shared_pending_sequence(shared: &Shared) -> Option<u64> {
        let pool = shared.pool.lock().unwrap();
        let slot = pool.pending?;
        let FrameSlot::Pending(frame) = &pool.slots[slot] else {
            panic!("pending slot state disagrees with pool index");
        };
        Some(frame.metadata.sequence)
    }

    #[derive(Default)]
    struct H264AccessUnit {
        nal_units: Vec<Vec<u8>>,
        fragmented_nal: Option<usize>,
    }

    impl H264AccessUnit {
        fn push_rtp_payload(&mut self, payload: &[u8]) {
            assert!(!payload.is_empty(), "empty H.264 RTP payload");
            match payload[0] & 0x1f {
                1..=23 => {
                    assert!(
                        self.fragmented_nal.is_none(),
                        "single NAL interrupted a fragmented NAL"
                    );
                    self.nal_units.push(payload.to_vec());
                }
                24 => {
                    assert!(
                        self.fragmented_nal.is_none(),
                        "STAP-A interrupted a fragmented NAL"
                    );
                    let mut remaining = &payload[1..];
                    while !remaining.is_empty() {
                        assert!(remaining.len() >= 2, "truncated STAP-A NAL length");
                        let length =
                            usize::from(u16::from_be_bytes(remaining[..2].try_into().unwrap()));
                        remaining = &remaining[2..];
                        assert!(length > 0, "empty STAP-A NAL");
                        assert!(remaining.len() >= length, "truncated STAP-A NAL");
                        self.nal_units.push(remaining[..length].to_vec());
                        remaining = &remaining[length..];
                    }
                }
                28 => {
                    assert!(payload.len() >= 2, "truncated FU-A header");
                    let nal_type = payload[1] & 0x1f;
                    let start = payload[1] & 0x80 != 0;
                    let end = payload[1] & 0x40 != 0;
                    assert_ne!(nal_type, 0, "FU-A has no NAL type");
                    if start {
                        assert!(
                            self.fragmented_nal.is_none(),
                            "FU-A start interrupted a fragmented NAL"
                        );
                        let index = self.nal_units.len();
                        let mut nal = Vec::with_capacity(payload.len() - 1);
                        nal.push((payload[0] & 0xe0) | nal_type);
                        nal.extend_from_slice(&payload[2..]);
                        self.nal_units.push(nal);
                        if !end {
                            self.fragmented_nal = Some(index);
                        }
                    } else {
                        let index = self
                            .fragmented_nal
                            .expect("FU-A continuation arrived without a start");
                        assert_eq!(self.nal_units[index][0] & 0x1f, nal_type);
                        self.nal_units[index].extend_from_slice(&payload[2..]);
                        if end {
                            self.fragmented_nal = None;
                        }
                    }
                }
                nal_type => panic!("unsupported H.264 RTP packetization type {nal_type}"),
            }
        }

        fn finish(self) -> Vec<Vec<u8>> {
            assert!(
                self.fragmented_nal.is_none(),
                "RTP marker arrived during a fragmented NAL"
            );
            assert!(!self.nal_units.is_empty(), "RTP access unit had no NALs");
            self.nal_units
        }
    }

    fn rtp_payload(packet: &[u8]) -> &[u8] {
        assert!(packet.len() >= 12, "truncated RTP header");
        assert_eq!(packet[0], 0x80, "FFmpeg emitted RTP header options");
        assert_eq!(packet[1] & 0x7f, 96, "unexpected RTP payload type");
        &packet[12..]
    }

    fn receive_frame(socket: &UdpSocket, mut access_unit: Option<&mut H264AccessUnit>) -> u32 {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut ssrc = None;
        let mut timestamp = None;
        while Instant::now() < deadline {
            let mut packet = [0_u8; 65_535];
            match socket.recv(&mut packet) {
                Ok(length) if length >= 12 => {
                    let packet = &packet[..length];
                    let packet_ssrc = u32::from_be_bytes(packet[8..12].try_into().unwrap());
                    let packet_timestamp = u32::from_be_bytes(packet[4..8].try_into().unwrap());
                    if let Some(previous) = ssrc {
                        assert_eq!(packet_ssrc, previous);
                    }
                    if let Some(previous) = timestamp {
                        assert_eq!(packet_timestamp, previous);
                    }
                    ssrc = Some(packet_ssrc);
                    timestamp = Some(packet_timestamp);
                    if let Some(access_unit) = access_unit.as_deref_mut() {
                        access_unit.push_rtp_payload(rtp_payload(packet));
                    }
                    if packet[1] & 0x80 != 0 {
                        return packet_ssrc;
                    }
                }
                Ok(_) => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) => {}
                Err(error) => panic!("RTP receive failed: {error}"),
            }
        }
        panic!("final RTP packet did not arrive while ffmpeg stdin remained open");
    }

    fn receive_frame_ssrc(socket: &UdpSocket) -> u32 {
        receive_frame(socket, None)
    }

    fn receive_first_rtp_access_unit(socket: &UdpSocket) -> Vec<Vec<u8>> {
        let mut access_unit = H264AccessUnit::default();
        receive_frame(socket, Some(&mut access_unit));
        access_unit.finish()
    }

    struct BitReader {
        bytes: Vec<u8>,
        position: usize,
    }

    impl BitReader {
        fn for_sps(nal: &[u8]) -> Self {
            assert_eq!(nal.first().map(|byte| byte & 0x1f), Some(7));
            let mut bytes = Vec::with_capacity(nal.len() - 1);
            let mut zeroes = 0;
            for &byte in &nal[1..] {
                if zeroes >= 2 && byte == 3 {
                    zeroes = 0;
                    continue;
                }
                bytes.push(byte);
                zeroes = if byte == 0 { zeroes + 1 } else { 0 };
            }
            Self { bytes, position: 0 }
        }

        fn bit(&mut self) -> bool {
            assert!(self.position < self.bytes.len() * 8, "truncated SPS");
            let byte = self.bytes[self.position / 8];
            let shift = 7 - self.position % 8;
            self.position += 1;
            byte & (1 << shift) != 0
        }

        fn bits(&mut self, count: usize) -> u32 {
            assert!(count <= 32);
            (0..count).fold(0, |value, _| (value << 1) | u32::from(self.bit()))
        }

        fn unsigned_exp_golomb(&mut self) -> u32 {
            let mut leading_zeroes = 0;
            while !self.bit() {
                leading_zeroes += 1;
                assert!(leading_zeroes < 32, "oversized SPS Exp-Golomb value");
            }
            if leading_zeroes == 0 {
                0
            } else {
                (1_u32 << leading_zeroes) - 1 + self.bits(leading_zeroes)
            }
        }
    }

    #[derive(Debug, PartialEq, Eq)]
    struct SpsColorContract {
        profile: u8,
        constraints: u8,
        level: u8,
        chroma_format: u32,
        full_range: bool,
        color_primaries: u8,
        transfer_characteristics: u8,
        matrix_coefficients: u8,
    }

    // This parser follows only the fields before the VUI color description in
    // the SPS produced by this fixed FFmpeg/libx264 configuration.
    fn parse_test_sps(nal: &[u8]) -> SpsColorContract {
        let mut bits = BitReader::for_sps(nal);
        let profile = bits.bits(8) as u8;
        let constraints = bits.bits(8) as u8;
        let level = bits.bits(8) as u8;
        bits.unsigned_exp_golomb();

        assert_eq!(profile, 244, "test parser only accepts High 4:4:4 SPS");
        let chroma_format = bits.unsigned_exp_golomb();
        if chroma_format == 3 {
            bits.bit();
        }
        bits.unsigned_exp_golomb();
        bits.unsigned_exp_golomb();
        bits.bit();
        assert!(
            !bits.bit(),
            "test parser does not accept custom SPS scaling matrices"
        );

        bits.unsigned_exp_golomb();
        assert_eq!(
            bits.unsigned_exp_golomb(),
            2,
            "test parser expects x264 pic_order_cnt_type 2"
        );
        bits.unsigned_exp_golomb();
        bits.bit();
        bits.unsigned_exp_golomb();
        bits.unsigned_exp_golomb();
        if !bits.bit() {
            bits.bit();
        }
        bits.bit();
        if bits.bit() {
            bits.unsigned_exp_golomb();
            bits.unsigned_exp_golomb();
            bits.unsigned_exp_golomb();
            bits.unsigned_exp_golomb();
        }
        assert!(bits.bit(), "SPS has no VUI parameters");

        if bits.bit() && bits.bits(8) == 255 {
            bits.bits(16);
            bits.bits(16);
        }
        if bits.bit() {
            bits.bit();
        }
        assert!(bits.bit(), "SPS VUI has no video signal type");
        bits.bits(3);
        let full_range = bits.bit();
        assert!(bits.bit(), "SPS VUI has no color description");
        let color_primaries = bits.bits(8) as u8;
        let transfer_characteristics = bits.bits(8) as u8;
        let matrix_coefficients = bits.bits(8) as u8;

        SpsColorContract {
            profile,
            constraints,
            level,
            chroma_format,
            full_range,
            color_primaries,
            transfer_characteristics,
            matrix_coefficients,
        }
    }

    #[test]
    fn spawn_failure_retries_retained_frame_without_another_submit() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let spawn_attempts = Arc::clone(&attempts);
        let encoder = encoder_with_spawner(
            Duration::from_millis(10),
            move |_, _, config, generation| {
                if spawn_attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                    Err(io::Error::other("injected spawn failure"))
                } else {
                    spawn_test_sink(config, generation)
                }
            },
        );

        submit_frame(&encoder, &frame(1, 1)).unwrap();

        assert!(matches!(
            wait_for_notification(&encoder),
            Notification::Submitted(FrameMetadata { sequence: 1, .. })
        ));
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        encoder.stop();
    }

    #[test]
    fn newer_pending_frame_replaces_retained_spawn_retry() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let spawn_attempts = Arc::clone(&attempts);
        let encoder = encoder_with_spawner(
            Duration::from_millis(200),
            move |_, _, config, generation| {
                if spawn_attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                    Err(io::Error::other("injected spawn failure"))
                } else {
                    spawn_test_sink(config, generation)
                }
            },
        );
        submit_frame(&encoder, &frame(1, 1)).unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        while attempts.load(Ordering::SeqCst) == 0
            || shared_pending_sequence(&encoder.shared) != Some(1)
        {
            assert!(Instant::now() < deadline, "failed frame was not requeued");
            thread::sleep(Duration::from_millis(1));
        }

        assert_eq!(
            submit_frame(&encoder, &frame(2, 1)).unwrap(),
            SubmitResult::ReplacedPending
        );
        assert!(matches!(
            wait_for_notification(&encoder),
            Notification::Submitted(FrameMetadata { sequence: 2, .. })
        ));
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        encoder.stop();
    }

    #[test]
    fn repeated_spawn_failures_publish_fatal_at_the_attempt_bound() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let spawn_attempts = Arc::clone(&attempts);
        let encoder = encoder_with_spawner(Duration::from_millis(1), move |_, _, _, _| {
            spawn_attempts.fetch_add(1, Ordering::SeqCst);
            Err(io::Error::other("injected persistent spawn failure"))
        });
        submit_frame(&encoder, &frame(1, 1)).unwrap();

        let Notification::Fatal(error) = wait_for_notification(&encoder) else {
            panic!("spawn exhaustion emitted frame metadata");
        };
        assert!(error.contains("after 6 attempts"));
        assert_eq!(
            attempts.load(Ordering::SeqCst),
            usize::from(MAX_CONSECUTIVE_SPAWN_FAILURES)
        );
        encoder.stop();
    }

    #[test]
    fn generation_change_and_stop_discard_spawn_retry_frame() {
        for stop in [false, true] {
            let shared = shared();
            let encoder = VideoEncoder {
                shared: Arc::clone(&shared),
                notifications: mpsc::sync_channel(1).1,
                notification_source: None,
                worker: None,
            };
            submit_frame(&encoder, &frame(1, 1)).unwrap();
            let (slot, encoding) = take_pending_frame(&shared).unwrap();
            if stop {
                shared.pool.lock().unwrap().stopping = true;
            } else {
                shared.pool.lock().unwrap().generation = 2;
            }

            requeue_after_spawn_failure(&shared, slot, encoding);

            let pool = shared.pool.lock().unwrap();
            assert!(pool.pending.is_none());
            assert!(pool.encoding.is_none());
            assert!(matches!(pool.slots[slot], FrameSlot::Free(_)));
            drop(pool);
        }
    }

    #[test]
    fn latest_pending_replacement_does_not_touch_encoding_buffers() {
        let shared = shared();
        let encoder = VideoEncoder {
            shared: Arc::clone(&shared),
            notifications: mpsc::sync_channel(1).1,
            notification_source: None,
            worker: None,
        };

        let first = frame(1, 1);
        assert_eq!(
            encoder.submit(captured(&first)).unwrap(),
            SubmitResult::Queued
        );
        let (first_slot, first_encoding) = take_pending_frame(&shared).unwrap();

        let second = frame(2, 1);
        assert_eq!(
            encoder.submit(captured(&second)).unwrap(),
            SubmitResult::Queued
        );
        let pending_slot = shared.pool.lock().unwrap().pending.unwrap();
        let (pending_pointer, pending_capacity) = {
            let pool = shared.pool.lock().unwrap();
            let FrameSlot::Pending(pending) = &pool.slots[pending_slot] else {
                panic!("latest frame was not pending");
            };
            (pending.pixels.as_ptr(), pending.pixels.capacity())
        };

        for sequence in 4..=103 {
            let replacement = frame(sequence, 1);
            assert_eq!(
                encoder.submit(captured(&replacement)).unwrap(),
                SubmitResult::ReplacedPending
            );
            let pool = shared.pool.lock().unwrap();
            let FrameSlot::Pending(pending) = &pool.slots[pending_slot] else {
                panic!("replacement frame was not pending");
            };
            assert_eq!(pending.pixels.as_ptr(), pending_pointer);
            assert_eq!(pending.pixels.capacity(), pending_capacity);
            assert_eq!(pending.metadata, replacement.metadata);
            assert_eq!(pending.config, replacement.config);
            assert_eq!(pending.pixels, replacement.pixels);
        }

        assert_eq!(first_encoding.metadata.sequence, 1);
        assert!(first_encoding.pixels.iter().all(|pixel| *pixel == 1));
        assert_ne!(first_encoding.pixels.as_ptr(), pending_pointer);
        {
            let pool = shared.pool.lock().unwrap();
            assert!(matches!(pool.slots[first_slot], FrameSlot::Encoding));
            assert!(matches!(pool.slots[pending_slot], FrameSlot::Pending(_)));
            assert_eq!(
                pool.slots
                    .iter()
                    .filter(|slot| matches!(slot, FrameSlot::Free(_)))
                    .count(),
                1
            );
        }

        release_slot(&shared, first_slot, first_encoding);
        let (latest_slot, latest) = take_pending_frame(&shared).unwrap();
        assert_eq!(latest_slot, pending_slot);
        assert_eq!(latest.metadata.sequence, 103);
        assert!(latest.pixels.iter().all(|pixel| *pixel == 103));
        release_slot(&shared, latest_slot, latest);
    }

    #[test]
    fn generation_changes_reject_stale_input_without_replacing_pending_storage() {
        let shared = shared();
        let encoder = VideoEncoder {
            shared: Arc::clone(&shared),
            notifications: mpsc::sync_channel(1).1,
            notification_source: None,
            worker: None,
        };

        let current = frame(1, 1);
        encoder.submit(captured(&current)).unwrap();
        let pending_slot = shared.pool.lock().unwrap().pending.unwrap();
        let pending_pointer = {
            let pool = shared.pool.lock().unwrap();
            let FrameSlot::Pending(pending) = &pool.slots[pending_slot] else {
                panic!("current frame was not pending");
            };
            pending.pixels.as_ptr()
        };

        let future = frame(2, 2);
        assert!(matches!(
            encoder.submit(captured(&future)),
            Err(VideoError::InvalidFrame)
        ));
        {
            let pool = shared.pool.lock().unwrap();
            let FrameSlot::Pending(pending) = &pool.slots[pending_slot] else {
                panic!("rejected input changed the pending slot");
            };
            assert_eq!(pending.metadata.sequence, 1);
            assert_eq!(pending.pixels.as_ptr(), pending_pointer);
        }

        encoder.set_generation(2).unwrap();
        let pool = shared.pool.lock().unwrap();
        assert!(pool.pending.is_none());
        let FrameSlot::Free(storage) = &pool.slots[pending_slot] else {
            panic!("old-generation pending storage was not released");
        };
        assert_eq!(storage.as_ptr(), pending_pointer);
    }

    #[test]
    fn generation_changes_are_positive_checked_increments() {
        let shared = shared();
        let encoder = VideoEncoder {
            shared: Arc::clone(&shared),
            notifications: mpsc::sync_channel(1).1,
            notification_source: None,
            worker: None,
        };

        assert!(matches!(
            encoder.set_generation(0),
            Err(VideoError::InvalidFrame)
        ));
        assert!(matches!(
            encoder.set_generation(1),
            Err(VideoError::InvalidFrame)
        ));
        assert!(matches!(
            encoder.set_generation(3),
            Err(VideoError::InvalidFrame)
        ));
        encoder.set_generation(2).unwrap();
        assert!(matches!(
            encoder.set_generation(2),
            Err(VideoError::InvalidFrame)
        ));
        shared.pool.lock().unwrap().generation = MAX_MEDIA_GENERATION;
        assert!(matches!(
            encoder.set_generation(MAX_MEDIA_GENERATION + 1),
            Err(VideoError::InvalidFrame)
        ));
        assert_eq!(shared.pool.lock().unwrap().generation, MAX_MEDIA_GENERATION);
        shared.pool.lock().unwrap().generation = u32::MAX;
        assert!(matches!(
            encoder.set_generation(1),
            Err(VideoError::InvalidFrame)
        ));
    }

    #[test]
    fn three_slot_storage_reallocates_for_a_larger_frame_then_stays_stable() {
        let shared = shared();
        let encoder = VideoEncoder {
            shared: Arc::clone(&shared),
            notifications: mpsc::sync_channel(1).1,
            notification_source: None,
            worker: None,
        };

        for sequence in 1..=SLOT_COUNT as u64 {
            let small = sized_frame(sequence, 1, 2, 2);
            encoder.submit(captured(&small)).unwrap();
            let (slot, stored) = take_pending_frame(&shared).unwrap();
            assert_eq!(slot, sequence as usize - 1);
            release_slot(&shared, slot, stored);
        }

        let mut pointers = [std::ptr::null(); SLOT_COUNT];
        let mut capacities = [0; SLOT_COUNT];
        for sequence in 4..=6 {
            let large = sized_frame(sequence, 1, 64, 64);
            encoder.submit(captured(&large)).unwrap();
            let (slot, stored) = take_pending_frame(&shared).unwrap();
            pointers[slot] = stored.pixels.as_ptr();
            capacities[slot] = stored.pixels.capacity();
            assert!(capacities[slot] >= large.pixels.len());
            release_slot(&shared, slot, stored);
        }
        assert_ne!(pointers[0], pointers[1]);
        assert_ne!(pointers[0], pointers[2]);
        assert_ne!(pointers[1], pointers[2]);

        for sequence in 7..=106 {
            let source = if sequence % 2 == 0 {
                sized_frame(sequence, 1, 64, 64)
            } else {
                sized_frame(sequence, 1, 2, 2)
            };
            encoder.submit(captured(&source)).unwrap();
            let (slot, reused) = take_pending_frame(&shared).unwrap();
            assert_eq!(reused.pixels.as_ptr(), pointers[slot]);
            assert_eq!(reused.pixels.capacity(), capacities[slot]);
            assert_eq!(reused.pixels, source.pixels);
            release_slot(&shared, slot, reused);
        }
    }

    #[test]
    fn partial_pipe_writes_finish_the_same_frame() {
        let (mut reader, mut writer, capacity) = nonblocking_pipe();
        let bytes: Vec<_> = (0..capacity * 3 + 17).map(|index| index as u8).collect();
        let drain = thread::spawn(move || {
            thread::sleep(Duration::from_millis(10));
            let mut received = Vec::new();
            reader.read_to_end(&mut received).unwrap();
            received
        });

        write_frame(&shared(), &mut writer, &bytes, 1).unwrap();
        drop(writer);

        assert_eq!(drain.join().unwrap(), bytes);
    }

    #[test]
    fn generation_change_abandons_a_partially_written_pipe_frame() {
        let (mut reader, mut writer, capacity) = nonblocking_pipe();
        writer.write_all(&vec![0xa5; capacity]).unwrap();
        let shared = shared();
        let controller_shared = Arc::clone(&shared);
        let controller = thread::spawn(move || {
            let mut filler = vec![0; capacity];
            reader.read_exact(&mut filler).unwrap();
            assert!(filler.iter().all(|byte| *byte == 0xa5));
            thread::sleep(Duration::from_millis(10));
            let changed_at = Instant::now();
            controller_shared.pool.lock().unwrap().generation = 2;
            (reader, changed_at)
        });
        let bytes = vec![0x5a; capacity * 2];

        let error = write_frame(&shared, &mut writer, &bytes, 1).unwrap_err();
        let returned_at = Instant::now();
        let (mut reader, changed_at) = controller.join().unwrap();
        drop(writer);
        let mut received = Vec::new();
        reader.read_to_end(&mut received).unwrap();

        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert!(!received.is_empty());
        assert!(received.len() < bytes.len());
        assert!(received.iter().all(|byte| *byte == 0x5a));
        assert!(returned_at.duration_since(changed_at) < Duration::from_millis(100));
    }

    #[test]
    fn stop_interrupts_a_blocked_pipe_write_within_the_poll_interval() {
        let (_reader, mut writer, capacity) = nonblocking_pipe();
        writer.write_all(&vec![0xa5; capacity]).unwrap();
        let shared = shared();
        let writer_shared = Arc::clone(&shared);
        let (started_tx, started_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            started_tx.send(()).unwrap();
            write_frame(&writer_shared, &mut writer, &[0x5a], 1)
        });
        started_rx.recv().unwrap();
        thread::sleep(Duration::from_millis(10));

        let stopped_at = Instant::now();
        shared.pool.lock().unwrap().stopping = true;
        let error = worker.join().unwrap().unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
        assert!(stopped_at.elapsed() < Duration::from_millis(100));
    }

    #[test]
    fn notification_wakeup_drains_a_burst_and_rearms_without_loss() {
        let shared = shared();
        let (sender, receiver) = mpsc::sync_channel(64);
        let (notification_wake, notification_source) = make_ping().unwrap();
        let mut event_loop = EventLoop::<Vec<u64>>::try_new().unwrap();
        event_loop
            .handle()
            .insert_source(notification_source, move |(), _, sequences| {
                while let Ok(notification) = receiver.try_recv() {
                    let Notification::Submitted(metadata) = notification else {
                        panic!("unexpected notification");
                    };
                    sequences.push(metadata.sequence);
                }
            })
            .unwrap();

        for sequence in 0..64 {
            assert!(send_notification(
                &shared,
                &sender,
                &notification_wake,
                Notification::Submitted(frame(sequence, 1).metadata),
            ));
        }
        let mut sequences = Vec::new();
        event_loop
            .dispatch(Duration::from_millis(100), &mut sequences)
            .unwrap();
        assert_eq!(sequences, (0..64).collect::<Vec<_>>());

        assert!(send_notification(
            &shared,
            &sender,
            &notification_wake,
            Notification::Submitted(frame(64, 1).metadata),
        ));
        event_loop
            .dispatch(Duration::from_millis(100), &mut sequences)
            .unwrap();
        assert_eq!(sequences.last(), Some(&64));
    }

    #[test]
    fn full_notification_queue_wakes_for_fatal_side_channel() {
        let shared = shared();
        let (sender, receiver) = mpsc::sync_channel(1);
        sender
            .send(Notification::Submitted(frame(1, 1).metadata))
            .unwrap();
        let (notification_wake, notification_source) = make_ping().unwrap();
        let mut event_loop = EventLoop::<usize>::try_new().unwrap();
        event_loop
            .handle()
            .insert_source(notification_source, move |(), _, drained| {
                while receiver.try_recv().is_ok() {
                    *drained += 1;
                }
            })
            .unwrap();

        assert!(!send_notification_until(
            &shared,
            &sender,
            &notification_wake,
            Notification::Submitted(frame(2, 1).metadata),
            Duration::ZERO,
        ));
        let mut drained = 0;
        event_loop
            .dispatch(Duration::from_millis(100), &mut drained)
            .unwrap();

        assert_eq!(drained, 1);
        assert_eq!(
            shared.pool.lock().unwrap().notification_failure.as_deref(),
            Some("video notification queue remained full")
        );
    }

    #[test]
    fn a_full_notification_queue_cannot_block_shutdown() {
        let shared = shared();
        let (sender, receiver) = mpsc::sync_channel(1);
        let (notification_wake, _) = make_ping().unwrap();
        sender
            .send(Notification::Fatal("occupy queue".into()))
            .unwrap();
        shared.pool.lock().unwrap().stopping = true;
        let started = Instant::now();

        assert!(!send_notification(
            &shared,
            &sender,
            &notification_wake,
            Notification::Submitted(frame(1, 1).metadata)
        ));
        assert!(started.elapsed() < Duration::from_millis(100));
        drop(receiver);
    }

    #[test]
    fn worker_inherits_blocked_signal_and_child_resets_mask_before_exec() {
        struct MaskGuard(SigSet);
        impl Drop for MaskGuard {
            fn drop(&mut self) {
                self.0.thread_set_mask().unwrap();
            }
        }

        let mut blocked = SigSet::empty();
        blocked.add(Signal::SIGTERM);
        let _mask_guard = MaskGuard(blocked.thread_swap_mask(SigmaskHow::SIG_BLOCK).unwrap());
        let (pid_tx, pid_rx) = mpsc::channel();
        let (status_tx, status_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            assert!(SigSet::thread_get_mask().unwrap().contains(Signal::SIGTERM));
            let mut command = Command::new("sleep");
            command.arg("30");
            reset_signal_mask_before_exec(&mut command);
            let mut child = command.spawn().unwrap();
            pid_tx.send(child.id()).unwrap();
            status_tx.send(child.wait().unwrap()).unwrap();
        });

        let child_pid = pid_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let child_pid = Pid::from_raw(i32::try_from(child_pid).unwrap());
        kill(child_pid, Signal::SIGTERM).unwrap();
        let status = match status_rx.recv_timeout(Duration::from_secs(2)) {
            Ok(status) => status,
            Err(error) => {
                let _ = kill(child_pid, Signal::SIGKILL);
                worker.join().unwrap();
                panic!("SIGTERM did not stop child with reset mask: {error}");
            }
        };
        worker.join().unwrap();
        assert_eq!(status.signal(), Some(Signal::SIGTERM as i32));
    }

    #[test]
    fn ffmpeg_flags_match_the_rtp_contract() {
        let args = ffmpeg_args(5000, frame(1, 1).config, 17);
        assert!(args.windows(2).any(|pair| pair == ["-payload_type", "96"]));
        assert!(args.windows(2).any(|pair| pair == ["-ssrc", "17"]));
        assert!(args.windows(2).any(|pair| {
            pair
                == [
                    "-x264-params",
                    "aud=1:repeat-headers=1:colorprim=bt709:transfer=iec61966-2-1:colormatrix=bt709:fullrange=on",
                ]
        }));
        assert_eq!(args.last().unwrap(), "rtp://127.0.0.1:5000?pkt_size=60000");
    }

    #[test]
    fn keyframes_use_quarter_the_nominal_frame_rate_without_changing_input_rate() {
        for (fps, interval) in [(60, 15), (30, 8), (1, 1)] {
            let mut config = frame(1, 1).config;
            config.fps = fps;
            let args = ffmpeg_args(5000, config, 17);
            for (flag, value) in [
                ("-framerate", fps),
                ("-g", interval),
                ("-keyint_min", interval),
            ] {
                assert!(
                    args.windows(2)
                        .any(|pair| pair[0] == flag && pair[1] == value.to_string())
                );
            }
        }
    }

    #[test]
    fn ffmpeg_converts_and_tags_desktop_srgb_consistently() {
        let args = ffmpeg_args(5000, frame(1, 1).config, 17);
        for pair in [
            ["-profile:v", "high444"],
            ["-pix_fmt", "yuv444p"],
            ["-vf", "scale=2:2:out_color_matrix=bt709:out_range=pc"],
            ["-colorspace", "bt709"],
            ["-color_primaries", "bt709"],
            ["-color_trc", "iec61966-2-1"],
            ["-color_range", "pc"],
        ] {
            assert!(args.windows(2).any(|actual| actual == pair));
        }
    }

    #[test]
    fn dimensions_enforce_level_and_four_k_budget() {
        assert_eq!(encoded_dimensions(3840, 2160, 100, 60), (3840, 2160));
        assert_eq!(frame(1, 1).config.validate().unwrap(), 16);
        let mut oversized = frame(1, 1).config;
        oversized.raw_width = 3842;
        oversized.raw_height = 2160;
        assert!(matches!(
            oversized.validate(),
            Err(VideoError::InvalidFrame)
        ));
    }

    #[test]
    fn every_same_generation_config_change_requests_a_new_generation() {
        let active = frame(1, 1).config;
        let changed_configs = [
            EncoderConfig {
                raw_width: 4,
                ..active
            },
            EncoderConfig {
                raw_height: 4,
                ..active
            },
            EncoderConfig {
                encoded_width: 4,
                ..active
            },
            EncoderConfig {
                encoded_height: 4,
                ..active
            },
            EncoderConfig { fps: 30, ..active },
            EncoderConfig {
                bitrate_kbps: 4_000,
                ..active
            },
        ];

        for changed in changed_configs {
            assert_eq!(
                encoder_transition(active, 1, changed, 1),
                EncoderTransition::RequestNewGeneration
            );
        }
        assert_eq!(
            encoder_transition(active, 1, changed_configs[0], 2),
            EncoderTransition::ReplaceForGeneration
        );
        assert_eq!(
            encoder_transition(active, 1, active, 1),
            EncoderTransition::Reuse
        );
    }

    #[test]
    #[ignore = "frame pool copy throughput probe; run explicitly"]
    fn pooled_copy_1824x848_frame_throughput() {
        let shared = shared();
        let encoder = VideoEncoder {
            shared: Arc::clone(&shared),
            notifications: mpsc::sync_channel(1).1,
            notification_source: None,
            worker: None,
        };
        let source = sized_frame(1, 1, 1824, 848);
        let frame_bytes = source.pixels.len();

        let mut pointers = [std::ptr::null(); SLOT_COUNT];
        let mut capacities = [0; SLOT_COUNT];
        for sequence in 1..=SLOT_COUNT as u64 {
            let metadata = FrameMetadata {
                sequence,
                capture_nanos: sequence,
                ..source.metadata.clone()
            };
            encoder
                .submit(CapturedFrame::new(&source.pixels, metadata, source.config))
                .unwrap();
            let (slot, warm) = take_pending_frame(&shared).unwrap();
            pointers[slot] = warm.pixels.as_ptr();
            capacities[slot] = warm.pixels.capacity();
            release_slot(&shared, slot, warm);
        }

        let copies = 300_u64;
        let started = Instant::now();
        for sequence in 4..copies + 4 {
            let metadata = FrameMetadata {
                sequence,
                capture_nanos: sequence,
                ..source.metadata.clone()
            };
            encoder
                .submit(CapturedFrame::new(&source.pixels, metadata, source.config))
                .unwrap();
            let (slot, copied) = take_pending_frame(&shared).unwrap();
            assert_eq!(copied.pixels.as_ptr(), pointers[slot]);
            assert_eq!(copied.pixels.capacity(), capacities[slot]);
            release_slot(&shared, slot, copied);
        }
        let elapsed = started.elapsed();
        let copied_bytes = frame_bytes as u64 * copies;

        eprintln!(
            "frame pool: {copies} copies of {frame_bytes} bytes in {elapsed:?} ({:.1} GiB/s)",
            copied_bytes as f64 / elapsed.as_secs_f64() / (1024.0 * 1024.0 * 1024.0)
        );
    }

    #[test]
    #[ignore = "raw pipe throughput probe; run explicitly"]
    fn raw_pipe_1824x864_frame_throughput() {
        let (mut reader, mut writer, capacity) = nonblocking_pipe();
        let bytes = vec![0x5a; 1824 * 864 * 4];
        let expected = bytes.len();
        let drain = thread::spawn(move || {
            let mut received = Vec::with_capacity(expected);
            reader.read_to_end(&mut received).unwrap();
            received
        });

        let started = Instant::now();
        write_frame(&shared(), &mut writer, &bytes, 1).unwrap();
        let elapsed = started.elapsed();
        drop(writer);
        let received = drain.join().unwrap();

        assert_eq!(received, bytes);
        eprintln!(
            "raw pipe: {expected} bytes through {capacity}-byte pipe in {elapsed:?} ({:.1} MiB/s)",
            expected as f64 / elapsed.as_secs_f64() / (1024.0 * 1024.0)
        );
    }

    #[test]
    #[ignore = "requires real FFmpeg with libx264; run the explicit ffmpeg_ suite"]
    fn ffmpeg_actual_sps_matches_gateway_avc1_f40034() {
        let socket = UdpSocket::bind(("127.0.0.1", 0)).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        let port = socket.local_addr().unwrap().port();
        let encoder = VideoEncoder::start("ffmpeg".into(), port).unwrap();
        submit_frame(&encoder, &real_frame(1, 1)).unwrap();
        assert!(matches!(
            wait_for_notification(&encoder),
            Notification::Submitted(FrameMetadata {
                generation: 1,
                sequence: 1,
                ..
            })
        ));

        let access_unit = receive_first_rtp_access_unit(&socket);
        let sps = access_unit
            .iter()
            .find(|nal| nal[0] & 0x1f == 7)
            .expect("first RTP access unit had no SPS");
        assert!(sps.len() >= 4, "SPS has no profile-level-id");
        assert_eq!(&sps[1..4], &[0xf4, 0x00, 0x34]);
        assert_eq!(
            format!("avc1.{:02X}{:02X}{:02X}", sps[1], sps[2], sps[3]),
            "avc1.F40034"
        );
        assert_eq!(
            parse_test_sps(sps),
            SpsColorContract {
                profile: 244,
                constraints: 0,
                level: 52,
                chroma_format: 3,
                full_range: true,
                color_primaries: 1,
                transfer_characteristics: 13,
                matrix_coefficients: 1,
            }
        );
        encoder.stop();
    }

    #[test]
    #[ignore = "requires real FFmpeg with libx264; run the explicit ffmpeg_ suite"]
    fn ffmpeg_preserves_largest_supported_generation_in_ssrc() {
        let socket = UdpSocket::bind(("127.0.0.1", 0)).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        let encoder =
            VideoEncoder::start("ffmpeg".into(), socket.local_addr().unwrap().port()).unwrap();
        let generation = MAX_MEDIA_GENERATION;
        encoder.shared.pool.lock().unwrap().generation = generation - 1;
        encoder.set_generation(generation).unwrap();
        submit_frame(&encoder, &real_frame(1, generation)).unwrap();
        assert!(matches!(
            wait_for_notification(&encoder),
            Notification::Submitted(_)
        ));
        assert_eq!(receive_frame_ssrc(&socket), generation);
        encoder.stop();
    }

    #[test]
    #[ignore = "requires real FFmpeg with libx264; run the explicit ffmpeg_ suite"]
    fn ffmpeg_stdin_has_one_mib_capacity() {
        let socket = UdpSocket::bind(("127.0.0.1", 0)).unwrap();
        let process = spawn_ffmpeg(
            "ffmpeg",
            socket.local_addr().unwrap().port(),
            real_frame(1, 1).config,
            1,
        )
        .unwrap();
        let capacity = fcntl(&process.input, FcntlArg::F_GETPIPE_SZ).unwrap();
        process.stop();
        assert_eq!(capacity, 1024 * 1024);
    }

    #[test]
    #[ignore = "requires real FFmpeg with libx264; run the explicit ffmpeg_ suite"]
    fn ffmpeg_emits_a_complete_rtp_frame_without_stdin_eof() {
        let socket = UdpSocket::bind(("127.0.0.1", 0)).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        let port = socket.local_addr().unwrap().port();
        let encoder = VideoEncoder::start("ffmpeg".into(), port).unwrap();
        submit_frame(&encoder, &real_frame(1, 1)).unwrap();
        assert!(matches!(
            wait_for_notification(&encoder),
            Notification::Submitted(FrameMetadata {
                generation: 1,
                sequence: 1,
                ..
            })
        ));

        assert_eq!(receive_frame_ssrc(&socket), 1);
        assert!(encoder.child_pid().is_some());
        encoder.stop();
    }

    #[test]
    #[ignore = "requires real FFmpeg with libx264; run the explicit ffmpeg_ suite"]
    fn ffmpeg_sparse_frames_resume_after_idle_at_thirty_and_sixty_fps() {
        for fps in [30, 60] {
            let socket = UdpSocket::bind(("127.0.0.1", 0)).unwrap();
            socket
                .set_read_timeout(Some(Duration::from_millis(100)))
                .unwrap();
            let port = socket.local_addr().unwrap().port();
            let encoder = VideoEncoder::start("ffmpeg".into(), port).unwrap();

            for (sequence, idle) in [
                (1, Duration::ZERO),
                (2, Duration::from_millis(250)),
                (3, Duration::from_secs(1)),
            ] {
                thread::sleep(idle);
                let frame = real_frame_at_fps(sequence, 1, fps);
                submit_frame(&encoder, &frame).unwrap();
                assert!(matches!(
                    wait_for_notification(&encoder),
                    Notification::Submitted(FrameMetadata {
                        generation: 1,
                        sequence: submitted,
                        fps: submitted_fps,
                        ..
                    }) if submitted == sequence && submitted_fps == fps
                ));
                assert_eq!(receive_frame_ssrc(&socket), 1);
                assert!(encoder.child_pid().is_some());
            }

            encoder.stop();
        }
    }

    #[test]
    #[ignore = "requires real FFmpeg with libx264; run the explicit ffmpeg_ suite"]
    fn ffmpeg_idle_exit_waits_for_a_new_generation_before_new_ssrc() {
        let socket = UdpSocket::bind(("127.0.0.1", 0)).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        let port = socket.local_addr().unwrap().port();
        let encoder = VideoEncoder::start("ffmpeg".into(), port).unwrap();
        submit_frame(&encoder, &real_frame(1, 1)).unwrap();
        assert!(matches!(
            wait_for_notification(&encoder),
            Notification::Submitted(FrameMetadata {
                generation: 1,
                sequence: 1,
                ..
            })
        ));
        let old_ssrc = receive_frame_ssrc(&socket);
        let pid = encoder.child_pid().expect("ffmpeg child was not recorded");

        kill(Pid::from_raw(pid as i32), Signal::SIGKILL).unwrap();
        assert_eq!(
            wait_for_notification(&encoder),
            Notification::RestartRequired { generation: 1 }
        );
        thread::sleep(Duration::from_millis(100));
        assert!(encoder.child_pid().is_none());

        encoder.set_generation(2).unwrap();
        submit_frame(&encoder, &real_frame(2, 2)).unwrap();
        assert!(matches!(
            wait_for_notification(&encoder),
            Notification::Submitted(FrameMetadata {
                generation: 2,
                sequence: 2,
                ..
            })
        ));
        let new_ssrc = receive_frame_ssrc(&socket);
        assert_eq!(old_ssrc, 1);
        assert_eq!(new_ssrc, 2);
        encoder.stop();
    }

    #[test]
    #[ignore = "requires real FFmpeg with libx264; run the explicit ffmpeg_ suite"]
    fn ffmpeg_config_change_waits_for_a_new_generation_before_new_ssrc() {
        let socket = UdpSocket::bind(("127.0.0.1", 0)).unwrap();
        socket
            .set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        let port = socket.local_addr().unwrap().port();
        let encoder = VideoEncoder::start("ffmpeg".into(), port).unwrap();
        submit_frame(&encoder, &real_frame(1, 1)).unwrap();
        assert!(matches!(
            wait_for_notification(&encoder),
            Notification::Submitted(FrameMetadata {
                generation: 1,
                sequence: 1,
                ..
            })
        ));
        let old_ssrc = receive_frame_ssrc(&socket);

        let mut changed = real_frame(2, 1);
        changed.config.bitrate_kbps = 4_000;
        submit_frame(&encoder, &changed).unwrap();
        assert_eq!(
            wait_for_notification(&encoder),
            Notification::RestartRequired { generation: 1 }
        );
        assert!(encoder.child_pid().is_none());

        let mut replacement = real_frame(3, 2);
        replacement.config.bitrate_kbps = 4_000;
        encoder.set_generation(2).unwrap();
        submit_frame(&encoder, &replacement).unwrap();
        assert!(matches!(
            wait_for_notification(&encoder),
            Notification::Submitted(FrameMetadata {
                generation: 2,
                sequence: 3,
                ..
            })
        ));
        let new_ssrc = receive_frame_ssrc(&socket);
        assert_eq!(old_ssrc, 1);
        assert_eq!(new_ssrc, 2);
        encoder.stop();
    }
}
