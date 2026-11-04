//! The gateway stream: pipe commands in, encoded frames and events out.
//!
//! `Stream` owns the encoder and the bookkeeping that ties its frames to the
//! gateway: the media generation and whether the gateway has cached its key
//! frame, frame and input sequence numbers, the encoding quality, the still
//! screen refresh, and the event pipe. It never touches the renderer, the
//! output or the render schedule: callers pass the rendered image and output
//! size in, and methods return what the compositor must do next, such as
//! render another frame or redraw the whole output.
//!
//! `State::apply_command` dispatches the gateway's commands: input goes to
//! the seat, and stream commands to `Stream`.
use std::time::Duration;
use std::time::Instant;

use anyhow::Context;
use anyhow::Result;
use anyhow::anyhow;
use smithay::backend::input::Axis;
use smithay::backend::input::AxisSource;
use smithay::backend::input::ButtonState;
use smithay::backend::input::InputTime;
use smithay::backend::input::KeyState;
use smithay::input::keyboard::KeyboardSource;
use smithay::input::pointer::AxisFrame;
use smithay::output::Scale;
use smithay::reexports::pixman::Image;
use smithay::utils::Physical;
use smithay::utils::Point;
use smithay::utils::Rectangle;
use smithay::wayland::text_input::TextInputSeat;
use waywire_encoder::latency;
use waywire_encoder::striped;
use waywire_encoder::striped::FrameCause;
use waywire_encoder::striped::StripedEncoder;
use waywire_encoder::striped::StripedFrame;
use waywire_encoder::video;
use waywire_encoder::video::CapturedFrame;
use waywire_encoder::video::EncoderConfig;
use waywire_encoder::video::Notification;
use waywire_encoder::video::VideoEncoder;
use waywire_encoder::video::encoded_dimensions;
use waywire_protocol::pipe;
use waywire_protocol::pipe::Command;
use waywire_protocol::pipe::Event;
use waywire_protocol::pipe::Fps;
use waywire_protocol::pipe::FrameDimension;
use waywire_protocol::pipe::FrameMetadata;
use waywire_protocol::pipe::FrameSize;
use waywire_protocol::pipe::Generation;
use waywire_protocol::pipe::InputSequence;
use waywire_protocol::pipe::Kbps;
use waywire_protocol::pipe::Quality;

use crate::FrameRequest;
use crate::delivery;
use crate::event_writer::EventSink;
use crate::event_writer::EventWriterError;
use crate::image_pixels;

/// Once nothing has changed for `REFRESH_DELAY`, the last frame is encoded
/// `REFRESH_FRAMES` more times. x264's constant rate factor codes a still frame's
/// remaining residual finer on each pass: a scrolled page gains about 2.5 dB after
/// six. The delay keeps the refresh out of typing and pointer cadences.
const REFRESH_DELAY: Duration = Duration::from_millis(500);
const REFRESH_FRAMES: u8 = 6;

/// The running encoder.
pub(crate) enum Video {
    Ffmpeg(VideoEncoder),
    Striped(StripedEncoder),
}

impl Video {
    fn set_generation(&self, generation: Generation) -> Result<(), video::VideoError> {
        match self {
            Self::Ffmpeg(video) => video.set_generation(generation),
            Self::Striped(video) => video.set_generation(generation),
        }
    }

    fn try_notification(&self) -> Option<Notification> {
        match self {
            Self::Ffmpeg(video) => video.try_notification(),
            Self::Striped(_) => None,
        }
    }

    pub(crate) fn take_notification_source(&mut self) -> Option<calloop::ping::PingSource> {
        match self {
            Self::Ffmpeg(video) => video.take_notification_source(),
            Self::Striped(video) => video.take_notification_source(),
        }
    }

    pub(crate) fn stop(self) {
        match self {
            Self::Ffmpeg(video) => video.stop(),
            Self::Striped(video) => video.stop(),
        }
    }

    /// Frames a still screen is refreshed over: x264 refines a repeated frame a little
    /// each pass, and the striped encoder spreads its stripes' refreshes.
    const fn refresh_frames(&self) -> u8 {
        match self {
            Self::Ffmpeg(_) => REFRESH_FRAMES,
            #[expect(clippy::cast_possible_truncation, reason = "a small constant")]
            Self::Striped(_) => striped::REFRESH_FRAMES as u8,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Refresh {
    Idle,
    Due { at: Instant, left: u8 },
}

/// What draining the encoder asks of the compositor.
#[derive(Default)]
pub(crate) struct Drained {
    /// Render the next frame: the event pipe dropped one, so the next is a key frame.
    pub(crate) frame: bool,
    /// Redraw the whole output: the encoder restarted on a new generation.
    pub(crate) redraw: bool,
    /// The first error, which ends the session.
    pub(crate) failure: Option<anyhow::Error>,
}

impl Drained {
    fn fail(&mut self, error: anyhow::Error) {
        if self.failure.is_none() {
            self.failure = Some(error);
        }
    }
}

pub(crate) struct Stream {
    video: Option<Video>,
    event_sink: EventSink,
    quality: Quality,
    generation: Generation,
    /// The generation whose key frame the gateway reports cached.
    acknowledged: Option<Generation>,
    sequence: u64,
    /// The latest browser input, stamped on frames and receipts so the gateway
    /// can match them to it.
    latest_input_sequence: Option<InputSequence>,
    /// The content hash of the last submitted frame when that frame was a forced full
    /// redraw, which reports the whole output damaged whether or not a pixel changed.
    forced_frame_hash: Option<u64>,
    refresh: Refresh,
}

impl Stream {
    pub(crate) fn new(
        video: Video,
        event_sink: EventSink,
        fps: Fps,
        bitrate: Kbps,
    ) -> Result<Self> {
        Ok(Self {
            video: Some(video),
            event_sink,
            quality: Quality::new(
                bitrate,
                fps,
                pipe::ScalePercent::new(100)?,
                pipe::Crf::new(27)?,
                pipe::Chroma::Yuv420,
            ),
            generation: Generation::new(1)?,
            acknowledged: None,
            sequence: 0,
            latest_input_sequence: None,
            forced_frame_hash: None,
            refresh: Refresh::Idle,
        })
    }

    pub(crate) const fn quality(&self) -> Quality {
        self.quality
    }

    /// A changed quality takes effect with the next generation; see
    /// [`Self::advance_generation`].
    pub(crate) const fn set_quality(&mut self, quality: Quality) {
        self.quality = quality;
    }

    pub(crate) const fn generation(&self) -> Generation {
        self.generation
    }

    /// Whether the gateway has cached a key frame of the current generation.
    /// Until it has, every frame is a forced full redraw.
    pub(crate) fn acknowledged(&self) -> bool {
        self.acknowledged == Some(self.generation)
    }

    pub(crate) const fn latest_input_sequence(&self) -> Option<InputSequence> {
        self.latest_input_sequence
    }

    /// Note browser input that later frames and receipts answer.
    pub(crate) fn record_input(&mut self, sequence: InputSequence) {
        self.latest_input_sequence = Some(sequence);
        tracing::debug!(
            target: "waywire_latency",
            stage = "compositor_input",
            input = sequence.get(),
            at_ns = latency::monotonic_nanos(),
        );
    }

    /// The encoder, until shutdown takes it to stop.
    pub(crate) const fn take_video(&mut self) -> Option<Video> {
        self.video.take()
    }

    pub(crate) fn emit(&self, event: &Event) -> Result<(), EventWriterError> {
        self.event_sink.send(event)
    }

    #[cfg(test)]
    pub(crate) fn take_events(&self) -> Vec<Event> {
        self.event_sink.take_events()
    }

    /// The gateway lost its key frame: the in-process encoder codes the next frame as one.
    /// The `FFmpeg` encoder's periodic key frames need no request.
    fn request_key(&self) {
        if let Some(Video::Striped(video)) = &self.video {
            video.request_key();
        }
    }

    /// Start a new generation, which the gateway must cache a key frame of.
    /// The caller redraws the whole output for it.
    pub(crate) fn advance_generation(&mut self) -> Result<()> {
        self.generation = Generation::new(
            self.generation
                .get()
                .checked_add(1)
                .context("media generation exhausted")?,
        )?;
        self.acknowledged = None;
        if let Some(video) = &self.video {
            video.set_generation(self.generation)?;
        }
        Ok(())
    }

    /// Record whether the gateway cached the key frame of `readiness.generation`.
    /// Returns true when it lost it, so the compositor must render a frame for
    /// the key frame this requests.
    pub(crate) fn keyframe_readiness(&mut self, readiness: pipe::KeyframeReadiness) -> bool {
        if readiness.generation() != self.generation {
            return false;
        }
        let (acknowledged, lost) = match readiness.state() {
            pipe::KeyframeState::Cached => (Some(readiness.generation()), false),
            pipe::KeyframeState::Missing => {
                self.request_key();
                (None, true)
            }
        };
        self.acknowledged = acknowledged;
        lost
    }

    /// Forward what the encoder produced: encoded frames and submission notices
    /// as events, restarts as a new generation.
    pub(crate) fn drain(&mut self) -> Drained {
        let mut drained = Drained::default();
        while let Some(frame) = match &self.video {
            Some(Video::Striped(video)) => video.try_frame(),
            _ => None,
        } {
            match frame {
                // A full stdout queue drops this frame rather than the session. The
                // stripes it carried are now out of step, so code a key frame next.
                Ok(frame) => {
                    let sequence = frame.metadata().sequence();
                    match self.event_sink.send(&Event::EncodedFrame(frame)) {
                        Ok(()) => latency::trace_frame("emitted", sequence, 0),
                        Err(EventWriterError::QueueFull) => {
                            self.request_key();
                            drained.frame = true;
                        }
                        Err(error) => drained.fail(error.into()),
                    }
                }
                Err(error) => drained.fail(anyhow!(error)),
            }
        }
        while let Some(notification) = self.video.as_ref().and_then(Video::try_notification) {
            match notification {
                Notification::Submitted(metadata) => {
                    if let Err(error) = self.event_sink.send(&Event::Frame(metadata)) {
                        drained.fail(error.into());
                    }
                }
                Notification::RestartRequired { generation } if generation == self.generation => {
                    drained.redraw = true;
                    if let Err(error) = self.advance_generation() {
                        drained.fail(error);
                    }
                }
                Notification::RestartRequired { .. } => {}
                Notification::Fatal(error) => drained.fail(anyhow!(error)),
            }
        }
        drained
    }

    /// Encode a rendered frame of an output `size` pixels large. `damage` is
    /// what the renderer redrew, or `None` when nothing changed. A `forced`
    /// redraw reports the whole output damaged. Returns whether the frame went
    /// to the encoder.
    pub(crate) fn rendered(
        &mut self,
        image: &Image<'static, 'static>,
        size: FrameSize,
        damage: Option<&[Rectangle<i32, Physical>]>,
        forced: bool,
        render_started_ns: u64,
    ) -> Result<bool> {
        if damage.is_none() && !forced {
            return Ok(false);
        }
        let full = [Rectangle::<i32, Physical>::from_size(
            (i32::try_from(size.width())?, i32::try_from(size.height())?).into(),
        )];
        let damage = if forced {
            &full[..]
        } else {
            damage.unwrap_or(&full)
        };
        // A forced redraw reports the whole output damaged. Hash only those, so
        // a redraw that changed no pixel is not encoded again.
        let hash = forced.then(|| xxhash_rust::xxh3::xxh3_64(image_pixels(image)));
        let unchanged = hash.is_some() && hash == self.forced_frame_hash && self.acknowledged();
        self.forced_frame_hash = hash;
        if unchanged {
            return Ok(false);
        }
        latency::trace_frame("render", self.sequence, render_started_ns);
        self.submit(image, size, damage, FrameCause::Damage)?;
        self.refresh = Refresh::Due {
            at: Instant::now() + REFRESH_DELAY,
            left: self.video.as_ref().map_or(0, Video::refresh_frames),
        };
        Ok(true)
    }

    /// When the screen has held still long enough to refresh it.
    pub(crate) const fn refresh_due(&self) -> Option<Instant> {
        match self.refresh {
            Refresh::Due { at, .. } => Some(at),
            Refresh::Idle => None,
        }
    }

    /// Encode the unchanged output again once the screen has held still.
    pub(crate) fn refresh(
        &mut self,
        image: &Image<'static, 'static>,
        size: FrameSize,
    ) -> Result<()> {
        let Refresh::Due { at, left } = self.refresh else {
            return Ok(());
        };
        let now = Instant::now();
        if now < at {
            return Ok(());
        }
        self.submit(image, size, &[], FrameCause::StillRefresh)?;
        self.refresh = match left.checked_sub(1) {
            Some(left @ 1..) => Refresh::Due {
                at: now + Duration::from_secs_f64(1.0 / f64::from(self.quality.fps().get())),
                left,
            },
            _ => Refresh::Idle,
        };
        Ok(())
    }

    fn submit(
        &mut self,
        image: &Image<'static, 'static>,
        size: FrameSize,
        damage: &[Rectangle<i32, Physical>],
        cause: FrameCause,
    ) -> Result<()> {
        let (width, height) = encoded_dimensions(
            size.width(),
            size.height(),
            self.quality.scale_percent(),
            self.quality.fps(),
        );
        let config = EncoderConfig::new(
            (
                FrameDimension::new(size.width().try_into()?)?,
                FrameDimension::new(size.height().try_into()?)?,
            ),
            (FrameDimension::new(width)?, FrameDimension::new(height)?),
            self.quality.fps(),
            self.quality.bitrate_kbps(),
            self.quality.crf(),
            self.quality.chroma(),
        );
        let metadata = FrameMetadata::new(
            self.generation,
            config.encoded_size().0,
            config.encoded_size().1,
            latency::monotonic_nanos(),
            self.sequence,
            self.latest_input_sequence,
            self.quality.fps(),
            self.quality.chroma(),
        );
        self.sequence = self
            .sequence
            .checked_add(1)
            .context("frame sequence exhausted")?;
        // ARGB8888 on little-endian hosts is packed BGRA; four-byte pixels have no row padding.
        match &self.video {
            Some(Video::Ffmpeg(video)) => {
                video.submit(CapturedFrame::new(image_pixels(image), metadata, config))?;
            }
            Some(Video::Striped(video)) => {
                let damage: Vec<_> = damage
                    .iter()
                    .map(|rectangle| {
                        rectangle.loc.y..rectangle.loc.y.saturating_add(rectangle.size.h)
                    })
                    .collect();
                video.submit(&StripedFrame {
                    pixels: image_pixels(image),
                    width: image.width(),
                    height: image.height(),
                    damage: &damage,
                    metadata,
                    crf: self.quality.crf(),
                    bitrate: self.quality.bitrate_kbps(),
                    cause,
                })?;
            }
            None => {}
        }
        Ok(())
    }
}

/// The compositor applies the stream's requests to the render schedule and
/// the session's lifetime, and dispatches the gateway's pipe commands.
impl crate::State {
    #[expect(
        clippy::too_many_lines,
        reason = "one direct match owns the pipe command vocabulary"
    )]
    pub(crate) fn apply_command(&mut self, command: Command) -> Result<()> {
        if let Some(sequence) = command.input_sequence() {
            self.stream.record_input(sequence);
        }
        let source = KeyboardSource::MAIN;
        match command {
            Command::PointerAbsolute(p) => self.pointer_motion(
                source,
                (
                    f64::from(p.x().get()) / 65535.0 * f64::from(self.size.width())
                        / self.output.current_scale().fractional_scale(),
                    f64::from(p.y().get()) / 65535.0 * f64::from(self.size.height())
                        / self.output.current_scale().fractional_scale(),
                )
                    .into(),
            ),
            Command::PointerRelative(p) => {
                if let Some(pointer) = self.seat.get_pointer() {
                    let scale = self.output.current_scale().fractional_scale();
                    let delta = Point::from((
                        f64::from(p.dx().get()) / scale,
                        f64::from(p.dy().get()) / scale,
                    ));
                    self.pointer_motion_with_delta(
                        source,
                        pointer.current_location() + delta,
                        Some(delta),
                    );
                }
            }
            Command::PointerButton(p) => self.pointer_button(
                source,
                p.button().evdev_code(),
                match p.state() {
                    pipe::ButtonState::Pressed => ButtonState::Pressed,
                    pipe::ButtonState::Released => ButtonState::Released,
                },
            ),
            Command::KeyboardKey(p) => match p.state() {
                pipe::KeyState::Pressed => {
                    self.keyboard_key(source, p.key().get(), KeyState::Pressed);
                }
                pipe::KeyState::Released => {
                    self.keyboard_key(source, p.key().get(), KeyState::Released);
                }
                pipe::KeyState::Repeated => self.keyboard_repeat(source, p.key().get()),
            },
            Command::ReleaseAll(_) => self.release_input(source),
            Command::InputLease(p) => self.set_browser_lease(p == pipe::InputControl::Browser),
            Command::PointerScroll(p) => {
                let frame = AxisFrame::new(InputTime::now())
                    .source(AxisSource::Finger)
                    .value(Axis::Horizontal, f64::from(p.dx().get()))
                    .value(Axis::Vertical, f64::from(p.dy().get()));
                self.pointer_axis(source, frame);
            }
            Command::PointerWheel(p) => {
                let mut frame = AxisFrame::new(InputTime::now()).source(AxisSource::Wheel);
                for (axis, value) in [
                    (Axis::Horizontal, p.dx().get()),
                    (Axis::Vertical, p.dy().get()),
                ] {
                    if value != 0.0 {
                        #[expect(
                            clippy::cast_possible_truncation,
                            reason = "wheel v120 is bounded by PointerDelta"
                        )]
                        let v120 = value.round() as i32;
                        frame = frame.v120(axis, v120).value(axis, f64::from(v120) / 8.0);
                    }
                }
                self.pointer_axis(source, frame);
            }
            Command::PointerScrollStop(p) => {
                let mut frame = AxisFrame::new(InputTime::now()).source(AxisSource::Finger);
                if p.dx().get() != 0.0 {
                    frame = frame.stop(Axis::Horizontal);
                }
                if p.dy().get() != 0.0 {
                    frame = frame.stop(Axis::Vertical);
                }
                self.pointer_axis(source, frame);
            }
            Command::Resize(p) => {
                self.size = p.size();
                let mode = smithay::output::Mode {
                    size: (
                        i32::try_from(p.size().width())?,
                        i32::try_from(p.size().height())?,
                    )
                        .into(),
                    refresh: i32::try_from(self.stream.quality().fps().get())? * 1000,
                };
                self.output.change_current_state(
                    Some(mode),
                    None,
                    Some(Scale::Fractional(f64::from(p.scale_v120().get()) / 120.0)),
                    None,
                );
                self.output.set_preferred(mode);
                self.space.map_output(&self.output, (0, 0));
                self.update_xwayland_scale()?;
                self.output_resized();
                self.update_cursor_image();
                self.capture.output_resized();
                if let Some(screencast) = &self.screencast {
                    screencast.resize(
                        (p.size().width(), p.size().height()),
                        self.stream.quality().fps().get(),
                    );
                }
                self.refresh_eis_regions();
                self.advance_generation()?;
                self.emit(Event::ResizeApplied(pipe::ResizeApplied::new(
                    p.request_id(),
                    p.size(),
                    p.scale_v120(),
                    self.stream.generation(),
                )));
            }
            Command::Quality(q) => {
                let current = self.stream.quality();
                if q != current {
                    if q.fps() != current.fps()
                        && let Some(mut mode) = self.output.current_mode()
                    {
                        mode.refresh = i32::try_from(q.fps().get())? * 1000;
                        self.output.set_preferred(mode);
                        self.output
                            .change_current_state(Some(mode), None, None, None);
                    }
                    self.stream.set_quality(q);
                    self.advance_generation()?;
                }
            }
            Command::ResetVideo(_) => self.advance_generation()?,
            Command::KeyframeReadiness(p) => {
                if self.stream.keyframe_readiness(p) {
                    self.frame_pending = FrameRequest::Pending;
                }
            }
            Command::Text(p) => {
                if matches!(p.action(), pipe::TextAction::Commit) {
                    let serial = self.delivery.begin_input(source, delivery::FocusCause::Key);
                    let (target, text_field) = self.keyboard_target();
                    self.finish_input(
                        source,
                        serial,
                        delivery::InputKind::Text,
                        &target,
                        Some(text_field),
                    );
                }
                let cursor = i32::try_from(p.text().as_str().len())?;
                let text_input = self.seat.text_input();
                let mut active = false;
                text_input.with_active_text_input(|text, _| {
                    active = true;
                    match p.action() {
                        pipe::TextAction::Commit => {
                            text.preedit_string(None, 0, 0);
                            text.commit_string(Some(p.text().as_str().into()));
                        }
                        pipe::TextAction::Preedit => {
                            text.preedit_string(Some(p.text().as_str().into()), cursor, cursor);
                        }
                    }
                });
                text_input.done(false);
                if !active && matches!(p.action(), pipe::TextAction::Commit) {
                    self.text_keyboard.queue(p.text().as_str().to_owned());
                    self.flush_text_keyboard();
                }
            }
            Command::Clipboard(text) => self.clipboard_set(&text),
        }
        Ok(())
    }

    #[expect(
        clippy::needless_pass_by_value,
        reason = "events are ephemeral messages consumed at emission"
    )]
    pub(crate) fn emit(&mut self, event: Event) {
        if let Err(error) = self.stream.emit(&event) {
            self.fail(error.into());
        }
    }

    fn advance_generation(&mut self) -> Result<()> {
        self.dirty = true;
        self.stream.advance_generation()
    }

    pub(crate) fn drain_video(&mut self) {
        let drained = self.stream.drain();
        if drained.frame {
            self.frame_pending = FrameRequest::Pending;
        }
        if drained.redraw {
            self.dirty = true;
        }
        if let Some(error) = drained.failure {
            self.fail(error);
        }
    }
}
