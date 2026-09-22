use std::fs::File;
use std::io::Read;
use std::net::UdpSocket;
use std::os::unix::process::ExitStatusExt;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use calloop::EventLoop;
use nix::fcntl::FcntlArg;
use nix::fcntl::OFlag;
use nix::fcntl::fcntl;
use nix::sys::signal::Signal;
use nix::sys::signal::kill;
use nix::unistd::Pid;
use nix::unistd::pipe;

use super::*;

fn generation(value: u32) -> Generation {
    Generation::new(value).expect("valid test generation")
}

fn dimension(value: u16) -> FrameDimension {
    FrameDimension::new(value).expect("valid test frame dimension")
}

fn fps(value: u32) -> Fps {
    Fps::new(value).expect("valid test frame rate")
}

fn kbps(value: u32) -> Kbps {
    Kbps::new(value).expect("valid test bitrate")
}

fn crf(value: u8) -> Crf {
    Crf::new(value).expect("valid test CRF")
}

fn wire_metadata(
    sequence: u64,
    generation: u32,
    width: FrameDimension,
    height: FrameDimension,
    input_sequence: Option<u32>,
    fps: Fps,
) -> FrameMetadata {
    FrameMetadata {
        generation: waywire_protocol::pipe::Generation::new(generation)
            .expect("test generation should be valid"),
        width,
        height,
        capture_nanos: sequence,
        sequence,
        input_sequence: input_sequence.map(|value| {
            waywire_protocol::pipe::InputSequence::new(value)
                .expect("test input sequence should be valid")
        }),
        fps,
        chroma: Chroma::Yuv444,
    }
}

fn frame(sequence: u64, generation: u32) -> RawFrame {
    RawFrame {
        pixels: vec![sequence.to_le_bytes()[0]; 16],
        metadata: wire_metadata(
            sequence,
            generation,
            dimension(2),
            dimension(2),
            None,
            fps(60),
        ),
        config: EncoderConfig {
            raw_width: dimension(2),
            raw_height: dimension(2),
            encoded_width: dimension(2),
            encoded_height: dimension(2),
            fps: fps(60),
            bitrate_kbps: kbps(8_000),
            crf: crf(23),
            chroma: Chroma::Yuv444,
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
        raw_width: dimension(u16::try_from(width).expect("test frame width should fit u16")),
        raw_height: dimension(u16::try_from(height).expect("test frame height should fit u16")),
        encoded_width: dimension(u16::try_from(width).expect("test frame width should fit u16")),
        encoded_height: dimension(u16::try_from(height).expect("test frame height should fit u16")),
        fps: fps(60),
        bitrate_kbps: kbps(8_000),
        crf: crf(23),
        chroma: Chroma::Yuv444,
    };
    RawFrame {
        pixels: vec![
            sequence.to_le_bytes()[0];
            config
                .validate()
                .expect("sized test frame should have a valid encoder config")
        ],
        metadata: wire_metadata(
            sequence,
            generation,
            config.encoded_width,
            config.encoded_height,
            Some(
                u32::try_from(sequence)
                    .expect("test frame sequence should fit the input sequence field"),
            ),
            config.fps,
        ),
        config,
    }
}

fn real_frame(sequence: u64, generation: u32) -> RawFrame {
    real_frame_at_fps(sequence, generation, 60)
}

fn real_frame_at_fps(sequence: u64, generation: u32, frame_rate: u32) -> RawFrame {
    let config = EncoderConfig {
        raw_width: dimension(320),
        raw_height: dimension(180),
        encoded_width: dimension(320),
        encoded_height: dimension(180),
        fps: fps(frame_rate),
        bitrate_kbps: kbps(8_000),
        crf: crf(23),
        chroma: Chroma::Yuv444,
    };
    RawFrame {
        pixels: vec![
            sequence.to_le_bytes()[0];
            config
                .validate()
                .expect("real-sized test frame should have a valid encoder config")
        ],
        metadata: wire_metadata(
            sequence,
            generation,
            config.encoded_width,
            config.encoded_height,
            None,
            config.fps,
        ),
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
            generation: generation(1),
            stopping: false,
            notification_failure: None,
            child_pid: None,
        }),
        work: Condvar::new(),
    })
}

fn nonblocking_pipe() -> (File, File, usize) {
    let (reader, writer) = pipe().expect("test pipe should open");
    let requested_capacity = 64 * 1024;
    if let Err(error) = fcntl(&writer, FcntlArg::F_SETPIPE_SZ(requested_capacity)) {
        // Pipe quotas may deny growth. These tests fill the measured capacity,
        // not the requested capacity, so the existing pipe size is sufficient.
        eprintln!("test pipe size request was refused; using measured capacity: {error}");
    }
    let capacity = usize::try_from(
        fcntl(&writer, FcntlArg::F_GETPIPE_SZ).expect("test pipe capacity should be readable"),
    )
    .expect("test pipe capacity should fit usize");
    let flags = OFlag::from_bits_truncate(
        fcntl(&writer, FcntlArg::F_GETFL).expect("test pipe flags should be readable"),
    );
    fcntl(&writer, FcntlArg::F_SETFL(flags | OFlag::O_NONBLOCK))
        .expect("test pipe should become nonblocking");
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

fn spawn_test_sink(config: EncoderConfig, generation: Generation) -> io::Result<EncoderProcess> {
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
    F: FnMut(&str, u16, EncoderConfig, Generation) -> io::Result<EncoderProcess> + Send + 'static,
{
    VideoEncoder::start_with_spawner(
        "test-encoder".into(),
        0,
        restart_delay,
        restart_delay,
        spawn,
    )
    .expect("test video encoder worker should start")
}

fn shared_pending_sequence(shared: &Shared) -> Option<u64> {
    let pool = shared
        .pool
        .lock()
        .expect("frame pool mutex should not be poisoned");
    let slot = pool.pending?;
    match &pool.slots[slot] {
        FrameSlot::Pending(frame) => Some(frame.metadata.sequence),
        FrameSlot::Free(_) | FrameSlot::Encoding => {
            panic!("pending slot state disagrees with pool index")
        }
    }
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
                    let length = usize::from(u16::from_be_bytes(
                        remaining[..2]
                            .try_into()
                            .expect("checked STAP-A length should contain two bytes"),
                    ));
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
        let mut packet = vec![0_u8; 65_535].into_boxed_slice();
        match socket.recv(&mut packet) {
            Ok(length) if length >= 12 => {
                let packet = &packet[..length];
                let packet_ssrc = u32::from_be_bytes(
                    packet[8..12]
                        .try_into()
                        .expect("checked RTP packet should contain an SSRC"),
                );
                let packet_timestamp = u32::from_be_bytes(
                    packet[4..8]
                        .try_into()
                        .expect("checked RTP packet should contain a timestamp"),
                );
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
    let profile = u8::try_from(bits.bits(8)).expect("8-bit SPS profile should fit u8");
    let constraints = u8::try_from(bits.bits(8)).expect("8-bit SPS constraints should fit u8");
    let level = u8::try_from(bits.bits(8)).expect("8-bit SPS level should fit u8");
    bits.unsigned_exp_golomb();

    assert!(
        matches!(profile, 100 | 244),
        "test parser only accepts High and High 4:4:4 SPS"
    );
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
    let color_primaries =
        u8::try_from(bits.bits(8)).expect("8-bit SPS color primaries should fit u8");
    let transfer_characteristics =
        u8::try_from(bits.bits(8)).expect("8-bit SPS transfer characteristics should fit u8");
    let matrix_coefficients =
        u8::try_from(bits.bits(8)).expect("8-bit SPS matrix coefficients should fit u8");

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
fn spawn_failure_retries_queued_frame_without_another_submit() {
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

    submit_frame(&encoder, &frame(1, 1)).expect("test frame should be accepted by the encoder");

    assert!(matches!(
        wait_for_notification(&encoder),
        Notification::Submitted(FrameMetadata { sequence: 1, .. })
    ));
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    encoder.stop();
}

#[test]
fn newer_pending_frame_replaces_queued_spawn_retry() {
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
    submit_frame(&encoder, &frame(1, 1)).expect("test frame should be accepted by the encoder");
    let deadline = Instant::now() + Duration::from_secs(1);
    while attempts.load(Ordering::SeqCst) == 0
        || shared_pending_sequence(&encoder.shared) != Some(1)
    {
        assert!(Instant::now() < deadline, "failed frame was not requeued");
        thread::sleep(Duration::from_millis(1));
    }

    assert_eq!(
        submit_frame(&encoder, &frame(2, 1)).expect("test frame should be accepted by the encoder"),
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
    submit_frame(&encoder, &frame(1, 1)).expect("test frame should be accepted by the encoder");

    let error = match wait_for_notification(&encoder) {
        Notification::Fatal(error) => error,
        Notification::Submitted(_) | Notification::RestartRequired { .. } => {
            panic!("spawn exhaustion emitted frame metadata")
        }
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
        submit_frame(&encoder, &frame(1, 1)).expect("test frame should be accepted by the encoder");
        let (slot, encoding) =
            take_pending_frame(&shared).expect("submitted test frame should occupy a pending slot");
        if stop {
            shared
                .pool
                .lock()
                .expect("frame pool mutex should not be poisoned")
                .stopping = true;
        } else {
            shared
                .pool
                .lock()
                .expect("frame pool mutex should not be poisoned")
                .generation = generation(2);
        }

        requeue_after_spawn_failure(&shared, slot, encoding);

        let pool = shared
            .pool
            .lock()
            .expect("frame pool mutex should not be poisoned");
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
        encoder
            .submit(captured(&first))
            .expect("test frame should be accepted by the encoder"),
        SubmitResult::Queued
    );
    let (first_slot, first_encoding) =
        take_pending_frame(&shared).expect("submitted test frame should occupy a pending slot");

    let second = frame(2, 1);
    assert_eq!(
        encoder
            .submit(captured(&second))
            .expect("test frame should be accepted by the encoder"),
        SubmitResult::Queued
    );
    let pending_slot = shared
        .pool
        .lock()
        .expect("frame pool mutex should not be poisoned")
        .pending
        .expect("submitted test frame should have a pending slot index");
    let (pending_pointer, pending_capacity) = {
        let pool = shared
            .pool
            .lock()
            .expect("frame pool mutex should not be poisoned");
        match &pool.slots[pending_slot] {
            FrameSlot::Pending(pending) => (pending.pixels.as_ptr(), pending.pixels.capacity()),
            FrameSlot::Free(_) | FrameSlot::Encoding => panic!("latest frame was not pending"),
        }
    };

    for sequence in 4..=103 {
        let replacement = frame(sequence, 1);
        assert_eq!(
            encoder
                .submit(captured(&replacement))
                .expect("test frame should be accepted by the encoder"),
            SubmitResult::ReplacedPending
        );
        let pool = shared
            .pool
            .lock()
            .expect("frame pool mutex should not be poisoned");
        match &pool.slots[pending_slot] {
            FrameSlot::Pending(pending) => {
                assert_eq!(pending.pixels.as_ptr(), pending_pointer);
                assert_eq!(pending.pixels.capacity(), pending_capacity);
                assert_eq!(pending.metadata, replacement.metadata);
                assert_eq!(pending.config, replacement.config);
                assert_eq!(pending.pixels, replacement.pixels);
            }
            FrameSlot::Free(_) | FrameSlot::Encoding => {
                panic!("replacement frame was not pending")
            }
        }
    }

    assert_eq!(first_encoding.metadata.sequence, 1);
    assert!(first_encoding.pixels.iter().all(|pixel| *pixel == 1));
    assert_ne!(first_encoding.pixels.as_ptr(), pending_pointer);
    {
        let pool = shared
            .pool
            .lock()
            .expect("frame pool mutex should not be poisoned");
        assert!(matches!(pool.slots[first_slot], FrameSlot::Encoding));
        assert!(matches!(pool.slots[pending_slot], FrameSlot::Pending(_)));
        assert_eq!(
            pool.slots
                .iter()
                .filter(|slot| matches!(slot, FrameSlot::Free(_)))
                .count(),
            0
        );
    }

    release_slot(&shared, first_slot, first_encoding);
    let (latest_slot, latest) =
        take_pending_frame(&shared).expect("submitted test frame should occupy a pending slot");
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
    encoder
        .submit(captured(&current))
        .expect("test frame should be accepted by the encoder");
    let pending_slot = shared
        .pool
        .lock()
        .expect("frame pool mutex should not be poisoned")
        .pending
        .expect("submitted test frame should have a pending slot index");
    let pending_pointer = {
        let pool = shared
            .pool
            .lock()
            .expect("frame pool mutex should not be poisoned");
        match &pool.slots[pending_slot] {
            FrameSlot::Pending(pending) => pending.pixels.as_ptr(),
            FrameSlot::Free(_) | FrameSlot::Encoding => panic!("current frame was not pending"),
        }
    };

    let future = frame(2, 2);
    assert!(matches!(
        encoder.submit(captured(&future)),
        Err(VideoError::InvalidFrame)
    ));
    {
        let pool = shared
            .pool
            .lock()
            .expect("frame pool mutex should not be poisoned");
        match &pool.slots[pending_slot] {
            FrameSlot::Pending(pending) => {
                assert_eq!(pending.metadata.sequence, 1);
                assert_eq!(pending.pixels.as_ptr(), pending_pointer);
            }
            FrameSlot::Free(_) | FrameSlot::Encoding => {
                panic!("rejected input changed the pending slot")
            }
        }
    }

    encoder
        .set_generation(generation(2))
        .expect("valid next media generation should be accepted");
    let pool = shared
        .pool
        .lock()
        .expect("frame pool mutex should not be poisoned");
    assert!(pool.pending.is_none());
    match &pool.slots[pending_slot] {
        FrameSlot::Free(storage) => assert_eq!(storage.as_ptr(), pending_pointer),
        FrameSlot::Pending(_) | FrameSlot::Encoding => {
            panic!("old-generation pending storage was not released")
        }
    }
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
        encoder.set_generation(generation(1)),
        Err(VideoError::InvalidFrame)
    ));
    assert!(matches!(
        encoder.set_generation(generation(3)),
        Err(VideoError::InvalidFrame)
    ));
    encoder
        .set_generation(generation(2))
        .expect("valid next media generation should be accepted");
    assert!(matches!(
        encoder.set_generation(generation(2)),
        Err(VideoError::InvalidFrame)
    ));
    shared
        .pool
        .lock()
        .expect("frame pool mutex should not be poisoned")
        .generation = generation(MAX_MEDIA_GENERATION);
    assert!(matches!(
        encoder.set_generation(generation(MAX_MEDIA_GENERATION + 1)),
        Err(VideoError::InvalidFrame)
    ));
    assert_eq!(
        shared
            .pool
            .lock()
            .expect("frame pool mutex should not be poisoned")
            .generation,
        generation(MAX_MEDIA_GENERATION)
    );
    shared
        .pool
        .lock()
        .expect("frame pool mutex should not be poisoned")
        .generation = generation(u32::MAX);
    assert!(matches!(
        encoder.set_generation(generation(1)),
        Err(VideoError::InvalidFrame)
    ));
}

#[test]
fn two_slot_storage_reallocates_for_a_larger_frame_then_stays_stable() {
    let shared = shared();
    let encoder = VideoEncoder {
        shared: Arc::clone(&shared),
        notifications: mpsc::sync_channel(1).1,
        notification_source: None,
        worker: None,
    };

    for sequence in 1..=SLOT_COUNT as u64 {
        let small = sized_frame(sequence, 1, 2, 2);
        encoder
            .submit(captured(&small))
            .expect("test frame should be accepted by the encoder");
        let (slot, stored) =
            take_pending_frame(&shared).expect("submitted test frame should occupy a pending slot");
        assert_eq!(
            slot,
            usize::try_from(sequence).expect("slot test sequence should fit usize") - 1
        );
        release_slot(&shared, slot, stored);
    }

    let mut pointers = [std::ptr::null(); SLOT_COUNT];
    let mut capacities = [0; SLOT_COUNT];
    for sequence in 3..=4 {
        let large = sized_frame(sequence, 1, 64, 64);
        encoder
            .submit(captured(&large))
            .expect("test frame should be accepted by the encoder");
        let (slot, stored) =
            take_pending_frame(&shared).expect("submitted test frame should occupy a pending slot");
        pointers[slot] = stored.pixels.as_ptr();
        capacities[slot] = stored.pixels.capacity();
        assert!(capacities[slot] >= large.pixels.len());
        release_slot(&shared, slot, stored);
    }
    assert_ne!(pointers[0], pointers[1]);

    for sequence in 5..=104 {
        // Both slots must see both sizes, rather than each slot always receiving
        // the same size on alternating submissions.
        let source = if sequence % 4 < 2 {
            sized_frame(sequence, 1, 64, 64)
        } else {
            sized_frame(sequence, 1, 2, 2)
        };
        encoder
            .submit(captured(&source))
            .expect("test frame should be accepted by the encoder");
        let (slot, reused) =
            take_pending_frame(&shared).expect("submitted test frame should occupy a pending slot");
        assert_eq!(reused.pixels.as_ptr(), pointers[slot]);
        assert_eq!(reused.pixels.capacity(), capacities[slot]);
        assert_eq!(reused.pixels, source.pixels);
        release_slot(&shared, slot, reused);
    }
}

#[test]
fn frame_write_io_error_retains_its_source() {
    let error = FrameWriteError::Io(io::Error::new(
        io::ErrorKind::BrokenPipe,
        "injected pipe failure",
    ));
    let source = std::error::Error::source(&error)
        .expect("frame write I/O failure should retain its source");
    assert_eq!(
        source.downcast_ref::<io::Error>().map(io::Error::kind),
        Some(io::ErrorKind::BrokenPipe)
    );
}

#[test]
fn partial_pipe_writes_finish_the_same_frame() {
    let (mut reader, mut writer, capacity) = nonblocking_pipe();
    let bytes: Vec<_> = (0..capacity * 3 + 17)
        .map(|index| index.to_le_bytes()[0])
        .collect();
    let drain = thread::spawn(move || {
        thread::sleep(Duration::from_millis(10));
        let mut received = Vec::new();
        reader
            .read_to_end(&mut received)
            .expect("test pipe should drain to EOF");
        received
    });

    write_frame(&shared(), &mut writer, &bytes, generation(1))
        .expect("test frame write should complete");
    drop(writer);

    assert_eq!(
        drain
            .join()
            .expect("pipe drain thread should finish cleanly"),
        bytes
    );
}

#[test]
fn generation_change_abandons_a_partially_written_pipe_frame() {
    let (mut reader, mut writer, capacity) = nonblocking_pipe();
    writer
        .write_all(&vec![0xa5; capacity])
        .expect("test pipe should accept its filler bytes");
    let shared = shared();
    let controller_shared = Arc::clone(&shared);
    let controller = thread::spawn(move || {
        let mut filler = vec![0; capacity];
        reader
            .read_exact(&mut filler)
            .expect("test pipe filler should drain completely");
        assert!(filler.iter().all(|byte| *byte == 0xa5));
        thread::sleep(Duration::from_millis(10));
        let changed_at = Instant::now();
        controller_shared
            .pool
            .lock()
            .expect("frame pool mutex should not be poisoned")
            .generation = generation(2);
        (reader, changed_at)
    });
    let bytes = vec![0x5a; capacity * 2];

    let error = write_frame(&shared, &mut writer, &bytes, generation(1))
        .expect_err("generation change should interrupt the frame write");
    let returned_at = Instant::now();
    let (mut reader, changed_at) = controller
        .join()
        .expect("pipe controller thread should finish cleanly");
    drop(writer);
    let mut received = Vec::new();
    reader
        .read_to_end(&mut received)
        .expect("test pipe should drain to EOF");

    assert!(matches!(
        error,
        FrameWriteError::GenerationChanged { observed, frame }
            if observed == generation(2) && frame == generation(1)
    ));
    assert!(!received.is_empty());
    assert!(received.len() < bytes.len());
    assert!(received.iter().all(|byte| *byte == 0x5a));
    assert!(returned_at.duration_since(changed_at) < Duration::from_millis(100));
}

#[test]
fn stop_interrupts_a_blocked_pipe_write_within_the_poll_interval() {
    let (reader_guard, mut writer, capacity) = nonblocking_pipe();
    writer
        .write_all(&vec![0xa5; capacity])
        .expect("test pipe should accept its filler bytes");
    let shared = shared();
    let writer_shared = Arc::clone(&shared);
    let (started_tx, started_rx) = mpsc::channel();
    let worker = thread::spawn(move || {
        started_tx
            .send(())
            .expect("worker should report that its write started");
        write_frame(&writer_shared, &mut writer, &[0x5a], generation(1))
    });
    started_rx
        .recv()
        .expect("test should observe the worker starting");
    thread::sleep(Duration::from_millis(10));

    let stopped_at = Instant::now();
    shared
        .pool
        .lock()
        .expect("frame pool mutex should not be poisoned")
        .stopping = true;
    let error = worker
        .join()
        .expect("blocked pipe writer thread should finish cleanly")
        .expect_err("stopping should interrupt the blocked frame write");

    assert!(matches!(error, FrameWriteError::Stopping));
    assert!(stopped_at.elapsed() < Duration::from_millis(100));
    drop(reader_guard);
}

#[test]
fn notification_wakeup_drains_a_burst_and_rearms_without_loss() {
    let shared = shared();
    let (sender, receiver) = mpsc::sync_channel(64);
    let (notification_wake, notification_source) =
        make_ping().expect("test notification ping should open");
    let mut event_loop = EventLoop::<Vec<u64>>::try_new().expect("test event loop should open");
    event_loop
        .handle()
        .insert_source(notification_source, move |(), (), sequences| {
            while let Ok(notification) = receiver.try_recv() {
                match notification {
                    Notification::Submitted(metadata) => sequences.push(metadata.sequence),
                    Notification::RestartRequired { .. } | Notification::Fatal(_) => {
                        panic!("unexpected notification")
                    }
                }
            }
        })
        .expect("notification source should register with the test event loop");

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
        .expect("test event loop should dispatch the notification");
    assert_eq!(sequences, (0..64).collect::<Vec<_>>());

    assert!(send_notification(
        &shared,
        &sender,
        &notification_wake,
        Notification::Submitted(frame(64, 1).metadata),
    ));
    event_loop
        .dispatch(Duration::from_millis(100), &mut sequences)
        .expect("test event loop should dispatch the notification");
    assert_eq!(sequences.last(), Some(&64));
}

#[test]
fn full_notification_queue_wakes_for_fatal_side_channel() {
    let shared = shared();
    let (sender, receiver) = mpsc::sync_channel(1);
    sender
        .send(Notification::Submitted(frame(1, 1).metadata))
        .expect("test notification should enter the queue");
    let (notification_wake, notification_source) =
        make_ping().expect("test notification ping should open");
    let mut event_loop = EventLoop::<usize>::try_new().expect("test event loop should open");
    event_loop
        .handle()
        .insert_source(notification_source, move |(), (), drained| {
            while receiver.try_recv().is_ok() {
                *drained += 1;
            }
        })
        .expect("notification source should register with the test event loop");

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
        .expect("test event loop should dispatch the notification");

    assert_eq!(drained, 1);
    assert_eq!(
        shared
            .pool
            .lock()
            .expect("frame pool mutex should not be poisoned")
            .notification_failure
            .as_deref(),
        Some("video notification queue remained full")
    );
}

#[test]
fn a_full_notification_queue_cannot_block_shutdown() {
    let shared = shared();
    let (sender, receiver) = mpsc::sync_channel(1);
    let (notification_wake, _) = make_ping().expect("test notification ping should open");
    sender
        .send(Notification::Fatal("occupy queue".into()))
        .expect("test notification should enter the queue");
    shared
        .pool
        .lock()
        .expect("frame pool mutex should not be poisoned")
        .stopping = true;
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
            self.0
                .thread_set_mask()
                .expect("original signal mask should be restored");
        }
    }

    let mut blocked = SigSet::empty();
    blocked.add(Signal::SIGTERM);
    let mask_guard = MaskGuard(
        blocked
            .thread_swap_mask(SigmaskHow::SIG_BLOCK)
            .expect("SIGTERM should be blocked for the test worker"),
    );
    let (pid_tx, pid_rx) = mpsc::channel();
    let (status_tx, status_rx) = mpsc::channel();
    let worker = thread::spawn(move || {
        assert!(
            SigSet::thread_get_mask()
                .expect("worker signal mask should be readable")
                .contains(Signal::SIGTERM)
        );
        let mut command = Command::new("sleep");
        command.arg("30");
        reset_signal_mask_before_exec(&mut command);
        let mut child = command.spawn().expect("test child process should start");
        pid_tx
            .send(child.id())
            .expect("worker should publish the child PID");
        status_tx
            .send(child.wait().expect("test child process should be waitable"))
            .expect("worker should publish the child exit status");
    });

    let child_pid = pid_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("test child should publish its PID before the deadline");
    let child_pid =
        Pid::from_raw(i32::try_from(child_pid).expect("test child PID should fit pid_t"));
    kill(child_pid, Signal::SIGTERM).expect("SIGTERM should reach the test child");
    let status = match status_rx.recv_timeout(Duration::from_secs(2)) {
        Ok(status) => status,
        Err(error) => {
            if let Err(cleanup_error) = kill(child_pid, Signal::SIGKILL) {
                // The timeout already fails this test, and the child may have
                // exited meanwhile. Keep joining the worker to finish reaping it.
                eprintln!("test child {child_pid} SIGKILL cleanup failed: {cleanup_error}");
            }
            worker
                .join()
                .expect("test worker thread should finish cleanly");
            panic!("SIGTERM did not stop child with reset mask: {error}");
        }
    };
    worker
        .join()
        .expect("test worker thread should finish cleanly");
    assert_eq!(status.signal(), Some(Signal::SIGTERM as i32));
    drop(mask_guard);
}

#[test]
fn ffmpeg_flags_match_the_rtp_contract() {
    let args = ffmpeg_args(5000, frame(1, 1).config, generation(17));
    assert!(args.windows(2).any(|pair| pair == ["-payload_type", "96"]));
    assert!(args.windows(2).any(|pair| pair == ["-ssrc", "17"]));
    assert!(args.windows(2).any(|pair| {
        pair
            == [
                "-x264-params",
                "aud=1:repeat-headers=1:colorprim=bt709:transfer=iec61966-2-1:colormatrix=bt709:fullrange=on",
            ]
    }));
    assert_eq!(
        args.last()
            .expect("FFmpeg argument list should include an output URL"),
        "rtp://127.0.0.1:5000?pkt_size=60000"
    );
}

#[test]
fn ffmpeg_uses_capped_crf_without_a_target_bitrate() {
    let args = ffmpeg_args(5000, frame(1, 1).config, generation(17));

    assert!(args.windows(2).any(|pair| pair == ["-crf", "23"]));
    assert!(args.windows(2).any(|pair| pair == ["-maxrate", "8000k"]));
    assert!(args.windows(2).any(|pair| pair == ["-bufsize", "16000k"]));
    assert!(!args.iter().any(|argument| argument == "-b:v"));
}

#[test]
fn keyframes_use_quarter_the_nominal_frame_rate_without_changing_input_rate() {
    for (rate, interval) in [(60, 15), (30, 8), (10, 3)] {
        let mut config = frame(1, 1).config;
        config.fps = fps(rate);
        let args = ffmpeg_args(5000, config, generation(17));
        for (flag, value) in [
            ("-framerate", rate),
            ("-g", interval),
            ("-keyint_min", interval),
        ] {
            assert!(
                args.windows(2)
                    .any(|pair| pair[0] == flag && pair[1] == value.to_string())
            );
        }
        assert!(!args.iter().any(|argument| argument == "-re"));
    }
}

#[test]
fn ffmpeg_converts_and_tags_desktop_srgb_for_each_chroma_choice() {
    for chroma in [Chroma::Yuv444, Chroma::Yuv420] {
        let mut config = frame(1, 1).config;
        config.chroma = chroma;
        let args = ffmpeg_args(5000, config, generation(17));
        for pair in [
            ["-profile:v", chroma.h264_profile().ffmpeg_profile()],
            ["-pix_fmt", chroma.ffmpeg_pixel_format()],
            ["-vf", "scale=2:2:out_color_matrix=bt709:out_range=pc"],
            ["-colorspace", "bt709"],
            ["-color_primaries", "bt709"],
            ["-color_trc", "iec61966-2-1"],
            ["-color_range", "pc"],
        ] {
            assert!(args.windows(2).any(|actual| actual == pair));
        }
    }
}

#[test]
fn ffmpeg_rgb_preserves_components_at_full_and_reduced_size() {
    for width in [320, 160] {
        let mut config = real_frame(1, 1).config;
        config.chroma = Chroma::Rgb;
        config.encoded_width = dimension(width);
        let args = ffmpeg_args(5000, config, generation(17));
        for pair in [
            ["-pixel_format", "bgr0"],
            ["-c:v", "libx264rgb"],
            ["-preset", "ultrafast"],
            ["-pix_fmt", "bgr0"],
            ["-colorspace", "rgb"],
            ["-color_range", "pc"],
            [
                "-x264-params",
                "aud=1:repeat-headers=1:colorprim=bt709:transfer=iec61966-2-1:colormatrix=gbr:fullrange=on",
            ],
        ] {
            assert!(args.windows(2).any(|actual| actual == pair));
        }
        let scale = format!("scale={}:{}", width, config.encoded_height.get());
        assert!(args.windows(2).any(|pair| pair == ["-vf", scale.as_str()]));
    }
}

#[test]
fn dimensions_enforce_level_and_four_k_budget() {
    assert_eq!(
        encoded_dimensions(
            3840,
            2160,
            waywire_protocol::pipe::ScalePercent::new(100)
                .expect("full test scale should be valid"),
            fps(60),
        ),
        (3840, 2160)
    );
    assert_eq!(
        frame(1, 1)
            .config
            .validate()
            .expect("fixed test frame should have a valid encoder config"),
        16
    );
    let mut oversized = frame(1, 1).config;
    oversized.raw_width = dimension(3842);
    oversized.raw_height = dimension(2160);
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
            raw_width: dimension(4),
            ..active
        },
        EncoderConfig {
            raw_height: dimension(4),
            ..active
        },
        EncoderConfig {
            encoded_width: dimension(4),
            ..active
        },
        EncoderConfig {
            encoded_height: dimension(4),
            ..active
        },
        EncoderConfig {
            fps: fps(30),
            ..active
        },
        EncoderConfig {
            bitrate_kbps: kbps(4_000),
            ..active
        },
        EncoderConfig {
            crf: crf(18),
            ..active
        },
        EncoderConfig {
            chroma: Chroma::Yuv420,
            ..active
        },
    ];

    for changed in changed_configs {
        assert_eq!(
            encoder_transition(active, generation(1), changed, generation(1)),
            EncoderTransition::RequestNewGeneration
        );
    }
    assert_eq!(
        encoder_transition(active, generation(1), changed_configs[0], generation(2)),
        EncoderTransition::ReplaceForGeneration
    );
    assert_eq!(
        encoder_transition(active, generation(1), active, generation(1)),
        EncoderTransition::Reuse
    );
}

#[test]
#[ignore = "requires real FFmpeg with libx264; run the explicit ffmpeg_ suite"]
fn ffmpeg_actual_sps_matches_each_protocol_profile() {
    for (chroma, profile, chroma_format, matrix_coefficients) in [
        (Chroma::Yuv444, 244, 3, 1),
        (Chroma::Yuv420, 100, 1, 1),
        (Chroma::Rgb, 244, 3, 0),
    ] {
        let socket =
            UdpSocket::bind(("127.0.0.1", 0)).expect("test RTP socket should bind to localhost");
        socket
            .set_read_timeout(Some(Duration::from_millis(100)))
            .expect("test RTP socket timeout should be configured");
        let port = socket
            .local_addr()
            .expect("bound test RTP socket should have a local address")
            .port();
        let encoder = VideoEncoder::start("ffmpeg".into(), port)
            .expect("FFmpeg-backed test encoder should start");
        let mut test_frame = real_frame(1, 1);
        test_frame.config.chroma = chroma;
        test_frame.metadata.chroma = chroma;
        submit_frame(&encoder, &test_frame).expect("test frame should be accepted by the encoder");
        assert!(matches!(
            wait_for_notification(&encoder),
            Notification::Submitted(FrameMetadata {
                generation,
                sequence: 1,
                ..
            }) if generation.get() == 1
        ));

        let access_unit = receive_first_rtp_access_unit(&socket);
        let sps = access_unit
            .iter()
            .find(|nal| nal[0] & 0x1f == 7)
            .expect("first RTP access unit had no SPS");
        assert!(sps.len() >= 4, "SPS has no profile-level-id");
        assert_eq!(sps[1], profile);
        assert_eq!(sps[3], 0x34);
        assert_eq!(
            format!("avc1.{:02X}{:02X}{:02X}", sps[1], sps[2], sps[3]),
            chroma.h264_profile().codec()
        );
        assert_eq!(
            parse_test_sps(sps),
            SpsColorContract {
                profile,
                constraints: sps[2],
                level: 52,
                chroma_format,
                full_range: true,
                color_primaries: 1,
                transfer_characteristics: 13,
                matrix_coefficients,
            }
        );
        encoder.stop();
    }
}

#[test]
#[ignore = "requires real FFmpeg with libx264; run the explicit ffmpeg_ suite"]
fn ffmpeg_preserves_largest_supported_generation_in_ssrc() {
    let socket =
        UdpSocket::bind(("127.0.0.1", 0)).expect("test RTP socket should bind to localhost");
    socket
        .set_read_timeout(Some(Duration::from_millis(100)))
        .expect("test RTP socket timeout should be configured");
    let encoder = VideoEncoder::start(
        "ffmpeg".into(),
        socket
            .local_addr()
            .expect("bound test RTP socket should have a local address")
            .port(),
    )
    .expect("FFmpeg-backed test encoder should start");
    let generation = MAX_MEDIA_GENERATION;
    encoder
        .shared
        .pool
        .lock()
        .expect("frame pool mutex should not be poisoned")
        .generation = Generation::new(generation - 1).expect("valid prior generation");
    encoder
        .set_generation(Generation::new(generation).expect("valid maximum generation"))
        .expect("valid next media generation should be accepted");
    submit_frame(&encoder, &real_frame(1, generation))
        .expect("test frame should be accepted by the encoder");
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
    let socket =
        UdpSocket::bind(("127.0.0.1", 0)).expect("test RTP socket should bind to localhost");
    let process = spawn_ffmpeg(
        "ffmpeg",
        socket
            .local_addr()
            .expect("bound test RTP socket should have a local address")
            .port(),
        real_frame(1, 1).config,
        generation(1),
    )
    .expect("FFmpeg test process should start");
    let capacity = fcntl(&process.input, FcntlArg::F_GETPIPE_SZ)
        .expect("FFmpeg stdin pipe capacity should be readable");
    process.stop();
    assert_eq!(capacity, 1024 * 1024);
}

#[test]
#[ignore = "requires real FFmpeg with libx264; run the explicit ffmpeg_ suite"]
fn ffmpeg_emits_a_complete_rtp_frame_without_stdin_eof() {
    let socket =
        UdpSocket::bind(("127.0.0.1", 0)).expect("test RTP socket should bind to localhost");
    socket
        .set_read_timeout(Some(Duration::from_millis(100)))
        .expect("test RTP socket timeout should be configured");
    let port = socket
        .local_addr()
        .expect("bound test RTP socket should have a local address")
        .port();
    let encoder = VideoEncoder::start("ffmpeg".into(), port)
        .expect("FFmpeg-backed test encoder should start");
    submit_frame(&encoder, &real_frame(1, 1))
        .expect("test frame should be accepted by the encoder");
    assert!(matches!(
        wait_for_notification(&encoder),
        Notification::Submitted(FrameMetadata {
            generation,
            sequence: 1,
            ..
        }) if generation.get() == 1
    ));

    assert_eq!(receive_frame_ssrc(&socket), 1);
    assert!(encoder.child_pid().is_some());
    encoder.stop();
}

#[test]
#[ignore = "requires real FFmpeg with libx264; run the explicit ffmpeg_ suite"]
fn ffmpeg_sparse_frames_resume_after_idle_at_thirty_and_sixty_fps() {
    for fps in [30, 60] {
        let socket =
            UdpSocket::bind(("127.0.0.1", 0)).expect("test RTP socket should bind to localhost");
        socket
            .set_read_timeout(Some(Duration::from_millis(100)))
            .expect("test RTP socket timeout should be configured");
        let port = socket
            .local_addr()
            .expect("bound test RTP socket should have a local address")
            .port();
        let encoder = VideoEncoder::start("ffmpeg".into(), port)
            .expect("FFmpeg-backed test encoder should start");

        for (sequence, idle) in [
            (1, Duration::ZERO),
            (2, Duration::from_millis(250)),
            (3, Duration::from_secs(1)),
        ] {
            thread::sleep(idle);
            let frame = real_frame_at_fps(sequence, 1, fps);
            submit_frame(&encoder, &frame).expect("test frame should be accepted by the encoder");
            assert!(matches!(
                wait_for_notification(&encoder),
                Notification::Submitted(FrameMetadata {
                    generation,
                    sequence: submitted,
                    fps: submitted_fps,
                    ..
                }) if generation.get() == 1
                    && submitted == sequence
                    && submitted_fps.get() == fps
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
    let socket =
        UdpSocket::bind(("127.0.0.1", 0)).expect("test RTP socket should bind to localhost");
    socket
        .set_read_timeout(Some(Duration::from_millis(100)))
        .expect("test RTP socket timeout should be configured");
    let port = socket
        .local_addr()
        .expect("bound test RTP socket should have a local address")
        .port();
    let encoder = VideoEncoder::start("ffmpeg".into(), port)
        .expect("FFmpeg-backed test encoder should start");
    submit_frame(&encoder, &real_frame(1, 1))
        .expect("test frame should be accepted by the encoder");
    assert!(matches!(
        wait_for_notification(&encoder),
        Notification::Submitted(FrameMetadata {
            generation,
            sequence: 1,
            ..
        }) if generation.get() == 1
    ));
    let old_ssrc = receive_frame_ssrc(&socket);
    let pid = encoder.child_pid().expect("ffmpeg child was not recorded");

    let pid = i32::try_from(pid).expect("FFmpeg child PID should fit pid_t");
    kill(Pid::from_raw(pid), Signal::SIGKILL).expect("SIGKILL should reach the FFmpeg child");
    assert_eq!(
        wait_for_notification(&encoder),
        Notification::RestartRequired {
            generation: generation(1)
        }
    );
    thread::sleep(Duration::from_millis(100));
    assert!(encoder.child_pid().is_none());

    encoder
        .set_generation(generation(2))
        .expect("valid next media generation should be accepted");
    submit_frame(&encoder, &real_frame(2, 2))
        .expect("test frame should be accepted by the encoder");
    assert!(matches!(
        wait_for_notification(&encoder),
        Notification::Submitted(FrameMetadata {
            generation,
            sequence: 2,
            ..
        }) if generation.get() == 2
    ));
    let new_ssrc = receive_frame_ssrc(&socket);
    assert_eq!(old_ssrc, 1);
    assert_eq!(new_ssrc, 2);
    encoder.stop();
}

#[test]
#[ignore = "requires real FFmpeg with libx264; run the explicit ffmpeg_ suite"]
fn ffmpeg_rgb_yuv_transitions_replace_the_color_matrix() {
    let socket = UdpSocket::bind(("127.0.0.1", 0)).expect("test socket should bind");
    socket
        .set_read_timeout(Some(Duration::from_millis(100)))
        .expect("test timeout");
    let encoder = VideoEncoder::start(
        "ffmpeg".into(),
        socket.local_addr().expect("test address").port(),
    )
    .expect("test encoder should start");
    for (index, (chroma, matrix, components)) in [
        (Chroma::Rgb, 0, 3),
        (Chroma::Yuv420, 1, 1),
        (Chroma::Rgb, 0, 3),
        (Chroma::Yuv444, 1, 3),
        (Chroma::Rgb, 0, 3),
    ]
    .into_iter()
    .enumerate()
    {
        let next = u32::try_from(index + 1).expect("small generation");
        if index > 0 {
            encoder
                .set_generation(generation(next))
                .expect("next generation");
        }
        let mut frame = real_frame(u64::from(next), next);
        frame.config.chroma = chroma;
        frame.metadata.chroma = chroma;
        frame.config.encoded_width = dimension(160);
        frame.config.encoded_height = dimension(90);
        frame.metadata.width = dimension(160);
        frame.metadata.height = dimension(90);
        submit_frame(&encoder, &frame).expect("frame accepted");
        assert!(matches!(
            wait_for_notification(&encoder),
            Notification::Submitted(_)
        ));
        let access_unit = receive_first_rtp_access_unit(&socket);
        let sps = access_unit
            .iter()
            .find(|nal| nal[0] & 0x1f == 7)
            .expect("SPS");
        let contract = parse_test_sps(sps);
        assert_eq!(contract.matrix_coefficients, matrix);
        assert_eq!(contract.chroma_format, components);
    }
    encoder.stop();
}

#[test]
#[ignore = "requires real FFmpeg with libx264; run the explicit ffmpeg_ suite"]
fn ffmpeg_chroma_reconfiguration_waits_for_a_new_generation_and_changes_sps() {
    let socket =
        UdpSocket::bind(("127.0.0.1", 0)).expect("test RTP socket should bind to localhost");
    socket
        .set_read_timeout(Some(Duration::from_millis(100)))
        .expect("test RTP socket timeout should be configured");
    let port = socket
        .local_addr()
        .expect("bound test RTP socket should have a local address")
        .port();
    let encoder = VideoEncoder::start("ffmpeg".into(), port)
        .expect("FFmpeg-backed test encoder should start");
    submit_frame(&encoder, &real_frame(1, 1))
        .expect("test frame should be accepted by the encoder");
    assert!(matches!(
        wait_for_notification(&encoder),
        Notification::Submitted(FrameMetadata {
            generation,
            sequence: 1,
            ..
        }) if generation.get() == 1
    ));
    let old_access_unit = receive_first_rtp_access_unit(&socket);
    let old_sps = old_access_unit
        .iter()
        .find(|nal| nal[0] & 0x1f == 7)
        .expect("4:4:4 access unit should contain an SPS");
    assert_eq!(parse_test_sps(old_sps).chroma_format, 3);

    let mut changed = real_frame(2, 1);
    changed.config.chroma = Chroma::Yuv420;
    changed.metadata.chroma = Chroma::Yuv420;
    submit_frame(&encoder, &changed).expect("test frame should be accepted by the encoder");
    assert_eq!(
        wait_for_notification(&encoder),
        Notification::RestartRequired {
            generation: generation(1)
        }
    );
    assert!(encoder.child_pid().is_none());

    let mut replacement = real_frame(3, 2);
    replacement.config.chroma = Chroma::Yuv420;
    replacement.metadata.chroma = Chroma::Yuv420;
    encoder
        .set_generation(generation(2))
        .expect("valid next media generation should be accepted");
    submit_frame(&encoder, &replacement).expect("test frame should be accepted by the encoder");
    assert!(matches!(
        wait_for_notification(&encoder),
        Notification::Submitted(FrameMetadata {
            generation,
            sequence: 3,
            ..
        }) if generation.get() == 2
    ));
    let new_access_unit = receive_first_rtp_access_unit(&socket);
    let new_sps = new_access_unit
        .iter()
        .find(|nal| nal[0] & 0x1f == 7)
        .expect("4:2:0 access unit should contain an SPS");
    assert_eq!(parse_test_sps(new_sps).chroma_format, 1);
    encoder.stop();
}
