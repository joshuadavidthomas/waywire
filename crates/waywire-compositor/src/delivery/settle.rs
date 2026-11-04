//! `WaitSettled`: wait until the screen, or one window's part of it, has
//! had no damage for a quiet window since an input.
//!
//! Damage is what the output damage tracker reports for each rendered frame:
//! committed client damage, Xwayland included, and the compositor's own
//! chrome and drag icon, after occlusion. The cursor is never drawn into
//! frames or screenshots, so it never counts, and neither does a change no
//! larger than the focused text field's caret, which blinks on a timer. A frame is timed
//! when it is rendered, which is when screenshots start to show it.
//!
//! Serials name inputs. Each serial is remembered with its time and with who
//! may see its receipt, so a caller can wait on its own input but cannot
//! learn when another process's input happened. Damage counts from the named
//! input, and the quiet window starts no earlier than the caller's own latest
//! input, so naming a key press also waits out its release.

use std::any::Any;
use std::collections::VecDeque;

use smithay::desktop::Space;
use smithay::desktop::Window;
use smithay::output::Output;
use smithay::reexports::wayland_protocols::wp::text_input::zv3::server::zwp_text_input_v3;
use smithay::reexports::wayland_protocols::wp::text_input::zv3::server::zwp_text_input_v3::ZwpTextInputV3;
use smithay::reexports::wayland_server::Resource;
use smithay::utils::Logical;
use smithay::utils::Physical;
use smithay::utils::Rectangle;
use smithay::utils::Size;
use smithay::wayland::seat::WaylandFocus;
use smithay::wayland::text_input::TextInputSeat;
use tokio::sync::oneshot;
use zbus::zvariant::OwnedValue;

use super::Delivery;
use super::SourceKind;
use super::monotonic_usec;
use crate::FrameRequest;
use crate::State;
use crate::toplevel_list::ToplevelList;

/// The quiet window when the caller names none.
pub(crate) const DEFAULT_QUIET_MS: u32 = 300;
pub(crate) const QUIET_MS: std::ops::RangeInclusive<u32> = 10..=10_000;
pub(crate) const DEFAULT_TIMEOUT_MS: u32 = 5_000;
/// Below the 25 s method-call timeout of libdbus and `GDBus` clients.
pub(crate) const TIMEOUT_MS: std::ops::RangeInclusive<u32> = 0..=20_000;
/// How far beyond a text field's cursor rectangle, in logical pixels, its
/// caret may draw on each side.
const CARET_MARGIN: i32 = 2;
/// Waits in progress at once, across every caller.
const MAX_WAITERS: usize = 64;
/// Serials remembered with their time.
const SERIALS: usize = 4096;
/// Damaged frames remembered, for inputs named after they happened.
const FRAMES: usize = 4096;
const HISTORY_USEC: u64 = 60_000_000;

pub(crate) type Reply =
    oneshot::Sender<zbus::fdo::Result<std::collections::HashMap<String, OwnedValue>>>;

/// A `WaitSettled` call's options, already range-checked.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SettleOptions {
    /// The input to measure from; `None` measures from when the compositor
    /// handles the call.
    serial: Option<u64>,
    quiet_ms: u32,
    timeout_ms: u32,
    /// A window id from `GetWindows`; `None` watches the whole output.
    window: Option<String>,
}

impl SettleOptions {
    pub(crate) fn new(
        serial: Option<u64>,
        quiet_ms: u32,
        timeout_ms: u32,
        window: Option<String>,
    ) -> Self {
        Self {
            serial,
            quiet_ms,
            timeout_ms,
            window,
        }
    }
}

/// Who may name a serial in `WaitSettled`: the same processes that receive
/// its signal.
#[derive(Clone, Debug)]
pub(super) enum Audience {
    Everyone,
    /// Browser input, which never appears on the bus.
    Nobody,
    /// The input source's process and its ancestors.
    Lineage(Vec<u32>),
}

impl Audience {
    pub(super) fn of(kind: Option<SourceKind>, lineage: &[u32]) -> Self {
        match kind {
            Some(SourceKind::Xtest) => Self::Everyone,
            Some(SourceKind::Viewer) => Self::Nobody,
            Some(_) => Self::Lineage(lineage.to_vec()),
            None => Self::Lineage(Vec::new()),
        }
    }

    pub(super) fn includes(&self, pid: Option<u32>) -> bool {
        match self {
            Self::Everyone => true,
            Self::Nobody => false,
            Self::Lineage(lineage) => pid.is_some_and(|pid| lineage.contains(&pid)),
        }
    }
}

/// What a serial names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum SerialKind {
    /// An input event, in its receipt.
    Input,
    /// A focus or window event.
    Notice,
}

#[derive(Debug)]
struct Issued {
    serial: u64,
    usec: u64,
    audience: Audience,
    kind: SerialKind,
}

#[derive(Debug)]
struct Frame {
    usec: u64,
    damage: Vec<Rectangle<i32, Physical>>,
}

/// How a wait ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Outcome {
    Settled,
    Timeout,
    /// The watched window closed.
    Closed,
}

impl Outcome {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Settled => "settled",
            Self::Timeout => "timeout",
            Self::Closed => "closed",
        }
    }
}

struct Waiter {
    since: u64,
    /// The caller's latest input before the call, if later than `since`:
    /// the quiet window starts after it too.
    after: u64,
    quiet: u64,
    deadline: u64,
    quiet_ms: u32,
    window: Option<String>,
    last_damage: Option<u64>,
    frames: u32,
    reply: Reply,
}

impl Waiter {
    /// When the current quiet window started.
    fn quiet_from(&self) -> u64 {
        self.last_damage.unwrap_or(self.since).max(self.after)
    }

    fn damaged(&mut self, usec: u64) {
        self.last_damage = Some(usec);
        self.frames = self.frames.saturating_add(1);
    }
}

/// The cursor rectangle a text-input-v3 object set, in its surface's
/// coordinates. Like the protocol's other state, it takes effect on commit.
#[derive(Debug)]
struct Caret {
    text_input: ZwpTextInputV3,
    pending: PendingCaret,
    current: Option<Rectangle<i32, Logical>>,
}

#[derive(Debug, Default)]
enum PendingCaret {
    #[default]
    Unchanged,
    Clear,
    Set(Rectangle<i32, Logical>),
}

/// Where the focused text field's caret can blink, in output pixels.
#[derive(Clone, Copy, Debug)]
pub(crate) struct CaretArea {
    size: Size<i32, Physical>,
    window: Rectangle<i32, Physical>,
}

impl CaretArea {
    /// The caret for a cursor rectangle `rect`, in window `window`, both
    /// in logical pixels, at `scale`. Clients report the cursor as a
    /// zero-width line and draw the caret around it, so the rectangle is
    /// widened by `CARET_MARGIN` on each side.
    pub(crate) fn new(
        rect: Rectangle<i32, Logical>,
        window: Rectangle<i32, Logical>,
        scale: f64,
    ) -> Self {
        let size = Rectangle::<i32, Logical>::from_size(
            rect.size + Size::from((2 * CARET_MARGIN, 2 * CARET_MARGIN)),
        );
        Self {
            size: super::windows::to_pixels(size, scale).size,
            window: super::windows::to_pixels(window, scale),
        }
    }

    /// Whether `rect` is no larger than the caret and inside its window.
    fn holds(self, rect: Rectangle<i32, Physical>) -> bool {
        rect.size.w <= self.size.w && rect.size.h <= self.size.h && self.window.contains_rect(rect)
    }
}

/// Settle state owned by the compositor loop.
#[derive(Default)]
pub(crate) struct Settle {
    carets: Vec<Caret>,
    /// The previous frame's pixels, kept while a text field has a caret.
    last: Option<Vec<u8>>,
    serials: VecDeque<Issued>,
    frames: VecDeque<Frame>,
    /// The newest frame dropped from `frames`: damage may have happened up
    /// to then without a record.
    forgotten: u64,
    /// When the latest frame was rendered, damaged or not. Commits before
    /// it are on screen.
    rendered: u64,
    waiters: Vec<Waiter>,
}

impl Settle {
    pub(super) fn issue(&mut self, serial: u64, audience: Audience, kind: SerialKind) {
        if self.serials.len() == SERIALS {
            self.serials.pop_front();
        }
        self.serials.push_back(Issued {
            serial,
            usec: monotonic_usec(),
            audience,
            kind,
        });
    }

    /// When `pid`'s latest input at or after `since` was dispatched.
    fn latest_input(&self, pid: Option<u32>, since: u64) -> Option<u64> {
        self.serials
            .iter()
            .rev()
            .take_while(|issued| issued.usec >= since)
            .find(|issued| issued.kind == SerialKind::Input && issued.audience.includes(pid))
            .map(|issued| issued.usec)
    }

    /// When `serial` was issued, if `pid` may name it. A serial older than
    /// the ones remembered starts at the oldest remembered one, which can
    /// only make the wait longer.
    fn issued(&self, serial: u64, latest: u64, pid: Option<u32>) -> zbus::fdo::Result<u64> {
        if serial == 0 || serial > latest {
            return Err(zbus::fdo::Error::InvalidArgs(format!(
                "serial {serial} has not been issued; the latest is {latest}"
            )));
        }
        let Some(oldest) = self.serials.front() else {
            return Ok(monotonic_usec());
        };
        let Some(index) = serial.checked_sub(oldest.serial) else {
            return Ok(oldest.usec);
        };
        let issued = usize::try_from(index)
            .ok()
            .and_then(|index| self.serials.get(index))
            .filter(|issued| issued.serial == serial)
            .ok_or_else(|| zbus::fdo::Error::InvalidArgs(format!("serial {serial} is unknown")))?;
        if !issued.audience.includes(pid) {
            return Err(zbus::fdo::Error::AccessDenied(format!(
                "serial {serial} is another process's input"
            )));
        }
        Ok(issued.usec)
    }
}

/// A wait that ended, for [`Delivery::answer_settled`] to answer.
pub(crate) struct Settled {
    waiter: Waiter,
    outcome: Outcome,
    now: u64,
}

impl Delivery {
    /// Track the cursor rectangle of each text-input-v3 object. Runs on
    /// every request before Smithay handles it, which keeps the rectangle to
    /// itself.
    pub(crate) fn observe_text_input(&mut self, resource: &dyn Any, request: &dyn Any) {
        let (Some(text_input), Some(request)) = (
            resource.downcast_ref::<ZwpTextInputV3>(),
            request.downcast_ref::<zwp_text_input_v3::Request>(),
        ) else {
            return;
        };
        let carets = &mut self.settle.carets;
        if let zwp_text_input_v3::Request::Destroy = request {
            carets.retain(|caret| caret.text_input != *text_input);
            return;
        }
        let index = carets
            .iter()
            .position(|caret| caret.text_input == *text_input)
            .unwrap_or_else(|| {
                carets.retain(|caret| caret.text_input.is_alive());
                carets.push(Caret {
                    text_input: text_input.clone(),
                    pending: PendingCaret::Unchanged,
                    current: None,
                });
                carets.len() - 1
            });
        let caret = &mut carets[index];
        match request {
            zwp_text_input_v3::Request::Enable | zwp_text_input_v3::Request::Disable => {
                caret.pending = PendingCaret::Clear;
            }
            zwp_text_input_v3::Request::SetCursorRectangle {
                x,
                y,
                width,
                height,
            } => {
                caret.pending =
                    PendingCaret::Set(Rectangle::new((*x, *y).into(), (*width, *height).into()));
            }
            zwp_text_input_v3::Request::Commit => match std::mem::take(&mut caret.pending) {
                PendingCaret::Unchanged => {}
                PendingCaret::Clear => caret.current = None,
                PendingCaret::Set(rect) => caret.current = Some(rect),
            },
            zwp_text_input_v3::Request::Destroy
            | zwp_text_input_v3::Request::SetSurroundingText { .. }
            | zwp_text_input_v3::Request::SetTextChangeCause { .. }
            | zwp_text_input_v3::Request::SetContentType { .. }
            | zwp_text_input_v3::Request::SetAvailableActions { .. }
            | zwp_text_input_v3::Request::ShowInputPanel
            | zwp_text_input_v3::Request::HideInputPanel
            | _ => {}
        }
    }

    /// The committed cursor rectangle of `text_input`, in its surface's
    /// coordinates.
    pub(crate) fn caret_rect(
        &self,
        text_input: &ZwpTextInputV3,
    ) -> Option<Rectangle<i32, Logical>> {
        self.settle
            .carets
            .iter()
            .find(|caret| caret.text_input == *text_input)?
            .current
    }

    /// Start a `WaitSettled` call at `now`. `pid` is the caller's process;
    /// `region` finds the part of the output a window and its popups cover,
    /// in output pixels, by its id, `None` when no window has it. Returns
    /// whether the wait started; a refused call is answered here.
    pub(crate) fn wait_settled(
        &mut self,
        pid: Option<u32>,
        options: SettleOptions,
        now: u64,
        region: impl Fn(&str) -> Option<Rectangle<i32, Physical>>,
        reply: Reply,
    ) -> bool {
        let since = match options.serial {
            None => Ok(now),
            Some(serial) => self.settle.issued(serial, self.last_serial(), pid),
        };
        let region = options.window.as_deref().map(region);
        let since = match since {
            Err(error) => Err(error),
            Ok(_) if matches!(region, Some(None)) => Err(zbus::fdo::Error::InvalidArgs(format!(
                "no window has id {:?}",
                options.window.as_deref().unwrap_or_default()
            ))),
            Ok(_) if self.settle.waiters.len() >= MAX_WAITERS => Err(
                zbus::fdo::Error::LimitsExceeded("too many settle waits in progress".into()),
            ),
            Ok(since) => Ok(since),
        };
        let since = match since {
            Ok(since) => since,
            Err(error) => {
                if reply.send(Err(error)).is_err() {
                    tracing::debug!("settle requester hung up");
                }
                return false;
            }
        };
        let region = region.flatten();
        let settle = &mut self.settle;
        let after = settle.latest_input(pid, since).unwrap_or(since).max(since);
        let mut waiter = Waiter {
            since,
            after,
            quiet: u64::from(options.quiet_ms) * 1000,
            deadline: now + u64::from(options.timeout_ms) * 1000,
            quiet_ms: options.quiet_ms,
            window: options.window,
            last_damage: None,
            frames: 0,
            reply,
        };
        if since <= settle.forgotten {
            waiter.last_damage = Some(settle.forgotten);
        }
        for frame in settle.frames.iter().filter(|frame| frame.usec >= since) {
            if touches(&frame.damage, region) {
                waiter.damaged(frame.usec);
            }
        }
        settle.waiters.push(waiter);
        true
    }

    /// Record a frame rendered at `now`: its damage in output pixels, the
    /// packed pixels it was rendered to, `width` to a row, and the parts of
    /// them that were drawn, `None` when all of them were. `caret` is where
    /// the focused text field's caret can blink; `region` finds a watched
    /// window's area, as in [`Self::wait_settled`].
    #[expect(
        clippy::too_many_arguments,
        reason = "one frame's facts, borrowed from the renderer"
    )]
    pub(crate) fn frame_rendered(
        &mut self,
        now: u64,
        damage: &[Rectangle<i32, Physical>],
        data: &[u8],
        width: usize,
        drawn: Option<&[Rectangle<i32, Physical>]>,
        caret: Option<CaretArea>,
        region: impl Fn(&str) -> Option<Rectangle<i32, Physical>>,
    ) {
        let settle = &mut self.settle;
        settle.rendered = now;
        let pixels = Pixels { data, width };
        // A blinking caret changes its pixels on a timer, not in answer to
        // input, so a change no larger than the caret in the field's window
        // is left out. GTK 4.8 reports where the caret is only when the
        // text changes, not after a click, so the change is matched by its
        // size, not its place. It also damages its whole surface for each
        // caret frame, so damage in the window is first narrowed to the
        // pixels that changed, which needs the previous frame.
        let last = settle
            .last
            .take()
            .filter(|last| last.len() == pixels.data.len());
        let damage: Vec<_> = damage
            .iter()
            .filter_map(|&rect| match (caret, &last) {
                (Some(caret), Some(last)) if rect.overlaps(caret.window) => {
                    changed_within(last, pixels, rect)
                }
                _ => Some(rect),
            })
            .filter(|rect| !caret.is_some_and(|caret| caret.holds(*rect)))
            .collect();
        settle.last = caret.map(|_| match (last, drawn) {
            (Some(mut last), Some(drawn)) => {
                for &rect in drawn {
                    for range in pixels.rows(rect) {
                        last[range.clone()].copy_from_slice(&pixels.data[range]);
                    }
                }
                last
            }
            (Some(mut last), None) => {
                last.copy_from_slice(pixels.data);
                last
            }
            (None, _) => pixels.data.to_vec(),
        });
        if damage.is_empty() {
            return;
        }
        for waiter in &mut settle.waiters {
            let region = waiter.window.as_deref().map(&region);
            // A window that closed is reported by the next poll.
            if let Some(None) = region {
                continue;
            }
            if touches(&damage, region.flatten()) {
                waiter.damaged(now);
            }
        }
        while settle.frames.len() >= FRAMES
            || settle
                .frames
                .front()
                .is_some_and(|frame| frame.usec + HISTORY_USEC < now)
        {
            if let Some(frame) = settle.frames.pop_front() {
                settle.forgotten = frame.usec;
            }
        }
        settle.frames.push_back(Frame { usec: now, damage });
    }

    /// Take every wait that settled, timed out or lost its window by `now`,
    /// in the order to answer them. `pending` says a frame is owed: commits
    /// since the last frame are not on screen yet and may damage it.
    /// `region` finds a watched window's area, as in [`Self::wait_settled`].
    pub(crate) fn poll_settled(
        &mut self,
        now: u64,
        pending: FrameRequest,
        region: impl Fn(&str) -> Option<Rectangle<i32, Physical>>,
    ) -> Vec<Settled> {
        let mut settled = Vec::new();
        let settle = &mut self.settle;
        if settle.waiters.is_empty() {
            return settled;
        }
        // A pending frame bounds how long the screen is known quiet.
        let quiet_until = if pending == FrameRequest::Idle {
            now
        } else {
            settle.rendered
        };
        let mut index = 0;
        while let Some(waiter) = settle.waiters.get(index) {
            let closed = waiter
                .window
                .as_deref()
                .is_some_and(|id| region(id).is_none());
            let outcome = if closed {
                Outcome::Closed
            } else if quiet_until >= waiter.quiet_from() + waiter.quiet {
                Outcome::Settled
            } else if now >= waiter.deadline {
                Outcome::Timeout
            } else {
                index += 1;
                continue;
            };
            let waiter = settle.waiters.swap_remove(index);
            settled.push(Settled {
                waiter,
                outcome,
                now,
            });
        }
        settled
    }

    /// Answer a wait [`Self::poll_settled`] took, after every signal
    /// already queued. Like `GetWindows`, the caller reports a pending
    /// window change first.
    pub(crate) fn answer_settled(&self, settled: Settled) {
        use super::bus::insert;
        let Settled {
            waiter,
            outcome,
            now,
        } = settled;
        let mut answer = std::collections::HashMap::new();
        insert(&mut answer, "result", outcome.as_str());
        insert(&mut answer, "time_usec", now);
        insert(&mut answer, "input_usec", waiter.since);
        insert(&mut answer, "damage_usec", waiter.last_damage.unwrap_or(0));
        insert(&mut answer, "frames", waiter.frames);
        insert(&mut answer, "quiet_ms", waiter.quiet_ms);
        insert(&mut answer, "serial", self.last_serial());
        let answer = super::bus::owned(answer);
        let reply = waiter.reply;
        self.flush(Box::new(move || {
            if reply.send(Ok(answer)).is_err() {
                tracing::debug!("settle requester hung up");
            }
        }));
    }

    /// When the loop must wake to answer a wait, in `CLOCK_MONOTONIC`
    /// microseconds. `pending` says a frame is owed.
    pub(crate) fn settle_wake(&self, pending: FrameRequest) -> Option<u64> {
        // A quiet window that ended with a frame pending is answered after
        // that frame, which wakes the loop itself.
        self.settle
            .waiters
            .iter()
            .map(|waiter| {
                let quiet = waiter.quiet_from() + waiter.quiet;
                if pending != FrameRequest::Idle && quiet <= monotonic_usec() {
                    waiter.deadline
                } else {
                    quiet.min(waiter.deadline)
                }
            })
            .min()
    }
}

/// A frame's packed 4-byte pixels, `width` pixels to a row.
#[derive(Clone, Copy)]
struct Pixels<'a> {
    data: &'a [u8],
    width: usize,
}

impl Pixels<'_> {
    /// The byte range of each of `rect`'s rows, clipped to the frame.
    fn rows(self, rect: Rectangle<i32, Physical>) -> impl Iterator<Item = std::ops::Range<usize>> {
        let stride = self.width * 4;
        let height = self.data.len().checked_div(stride).unwrap_or(0);
        let clip = |value: i32, limit: usize| usize::try_from(value.max(0)).unwrap_or(0).min(limit);
        let (left, right) = (
            clip(rect.loc.x, self.width),
            clip(rect.loc.x + rect.size.w, self.width),
        );
        let (top, bottom) = (
            clip(rect.loc.y, height),
            clip(rect.loc.y + rect.size.h, height),
        );
        (top..bottom).map(move |row| row * stride + left * 4..row * stride + right * 4)
    }
}

/// The smallest rectangle inside `rect` that holds every pixel that differs
/// between `last` and `now`, or `None` when none does.
fn changed_within(
    last: &[u8],
    now: Pixels<'_>,
    rect: Rectangle<i32, Physical>,
) -> Option<Rectangle<i32, Physical>> {
    let mut bounds: Option<(usize, usize, usize, usize)> = None;
    for (index, range) in now.rows(rect).enumerate() {
        let (old, new) = (&last[range.clone()], &now.data[range]);
        if old == new {
            continue;
        }
        let Some(first) = old.chunks(4).zip(new.chunks(4)).position(|(a, b)| a != b) else {
            continue;
        };
        let after = old
            .chunks(4)
            .zip(new.chunks(4))
            .rposition(|(a, b)| a != b)?
            + 1;
        bounds = Some(match bounds {
            None => (first, after, index, index + 1),
            Some((left, right, top, _)) => (left.min(first), right.max(after), top, index + 1),
        });
    }
    let (left, right, top, bottom) = bounds?;
    let at = |offset: usize, start: i32| start.max(0) + i32::try_from(offset).unwrap_or(i32::MAX);
    let loc = (at(left, rect.loc.x), at(top, rect.loc.y));
    Some(Rectangle::new(
        loc.into(),
        (
            at(right, rect.loc.x) - loc.0,
            at(bottom, rect.loc.y) - loc.1,
        )
            .into(),
    ))
}

/// Whether `damage` reaches `region`, or anywhere when there is no region.
fn touches(damage: &[Rectangle<i32, Physical>], region: Option<Rectangle<i32, Physical>>) -> bool {
    match region {
        None => !damage.is_empty(),
        Some(region) => damage.iter().any(|rect| rect.overlaps(region)),
    }
}

impl State {
    /// Whether a frame is owed: commits since the last frame are not on
    /// screen yet.
    fn frame_owed(&self) -> FrameRequest {
        self.frame_request
    }

    /// The focused text field's caret, in output pixels. `None` unless the
    /// field is in a window's main surface.
    fn caret(&self) -> Option<CaretArea> {
        let mut active = None;
        self.seat
            .text_input()
            .with_active_text_input(|text_input, surface| {
                active = Some((text_input.clone(), surface.clone()));
            });
        let (text_input, surface) = active?;
        let rect = self.delivery.caret_rect(&text_input)?;
        let window = self
            .space
            .elements()
            .find(|window| window.wl_surface().is_some_and(|own| *own == surface))?;
        let scale = self.output.current_scale().fractional_scale();
        Some(CaretArea::new(
            rect,
            self.space.element_bbox(window)?,
            scale,
        ))
    }

    /// Start a `WaitSettled` call. `pid` is the caller's process.
    pub(crate) fn wait_settled(&mut self, pid: Option<u32>, options: SettleOptions, reply: Reply) {
        let now = monotonic_usec();
        let (space, toplevel_list, output) = (&self.space, &self.toplevel_list, &self.output);
        if self.delivery.wait_settled(
            pid,
            options,
            now,
            |id| settle_region(space, toplevel_list, output, id),
            reply,
        ) {
            self.poll_settled(now);
        }
    }

    /// Record a rendered frame: its damage in output pixels, the image it
    /// was rendered to and the parts of it that were drawn, `None` when all
    /// of it was.
    pub(crate) fn frame_rendered(
        &mut self,
        now: u64,
        damage: &[Rectangle<i32, Physical>],
        image: &smithay::reexports::pixman::Image<'static, 'static>,
        drawn: Option<&[Rectangle<i32, Physical>]>,
    ) {
        let caret = self.caret();
        let (space, toplevel_list, output) = (&self.space, &self.toplevel_list, &self.output);
        self.delivery.frame_rendered(
            now,
            damage,
            crate::image_pixels(image),
            image.width(),
            drawn,
            caret,
            |id| settle_region(space, toplevel_list, output, id),
        );
    }

    /// Answer every wait that settled, timed out or lost its window by
    /// `now`.
    pub(crate) fn poll_settled(&mut self, now: u64) {
        let pending = self.frame_owed();
        let (space, toplevel_list, output) = (&self.space, &self.toplevel_list, &self.output);
        let settled = self.delivery.poll_settled(now, pending, |id| {
            settle_region(space, toplevel_list, output, id)
        });
        for settled in settled {
            // Like GetWindows: report a pending window change first.
            self.publish_window_list();
            self.delivery.answer_settled(settled);
        }
    }

    /// When the loop must wake to answer a wait, in `CLOCK_MONOTONIC`
    /// microseconds.
    pub(crate) fn settle_wake(&self) -> Option<u64> {
        self.delivery.settle_wake(self.frame_owed())
    }
}

/// The part of the output that window `id` and its popups cover, in output
/// pixels; `None` when no window has that id.
fn settle_region(
    space: &Space<Window>,
    toplevel_list: &ToplevelList,
    output: &Output,
    id: &str,
) -> Option<Rectangle<i32, Physical>> {
    let window = space
        .elements()
        .find(|window| toplevel_list.identifier(window).as_deref() == Some(id))?;
    let location = space.element_location(window)?;
    let (_, frame) = super::windows::bounds(window, location);
    let mut popups = window.bbox_with_popups();
    popups.loc += location - window.geometry().loc;
    let scale = output.current_scale().fractional_scale();
    Some(super::windows::to_pixels(frame.merge(popups), scale))
}
