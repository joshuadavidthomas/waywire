//! Drag-and-drop. Wayland drags run Smithay's `DnDGrab`; X11 drags enter
//! through the XWM's XDND bridge, which installs the same grab. Both reach
//! Wayland targets through `wl_data_device` and X11 targets through XDND.
//!
//! A drag ends with a drop only when the input source holding the button
//! releases it. A browser takeover or handback, a disconnected agent device,
//! and the source window's client exiting cancel the drag instead.
use std::sync::Arc;

use smithay::backend::input::InputTime;
use smithay::desktop::PopupPointerGrab;
use smithay::input::Seat;
use smithay::input::SeatHandler;
use smithay::input::dnd::DnDGrab;
use smithay::input::dnd::DndFocus;
use smithay::input::dnd::DndGrabHandler;
use smithay::input::dnd::DndTarget;
use smithay::input::dnd::GrabType;
use smithay::input::dnd::OfferData;
use smithay::input::dnd::Source;
use smithay::input::pointer::ClickGrab;
use smithay::input::pointer::CursorIcon;
use smithay::input::pointer::CursorImageStatus;
use smithay::input::pointer::Focus;
use smithay::input::pointer::MotionEvent;
use smithay::reexports::wayland_server::DisplayHandle;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::utils::Logical;
use smithay::utils::Point;
use smithay::utils::SERIAL_COUNTER;
use smithay::utils::Serial;
use smithay::wayland::selection::data_device::WaylandDndGrabHandler;
use smithay::wayland::selection::data_device::WlOfferData;
use smithay::xwayland::xwm::XwmOfferData;

use crate::FrameRequest;
use crate::State;
use crate::focus::PointerFocus;

/// A drag started by a Wayland client. X11 sources draw their own icon
/// windows and set their own cursor.
pub(crate) struct Drag {
    pub(crate) icon: Option<WlSurface>,
    /// The icon's offset from the pointer, moved by the icon's attach offsets.
    pub(crate) offset: Point<i32, Logical>,
}

pub(crate) enum Offer<S: Source> {
    Wayland(WlOfferData<S>),
    X11(XwmOfferData<S>),
}

impl<S: Source> OfferData for Offer<S> {
    fn disable(&self) {
        match self {
            Self::Wayland(offer) => offer.disable(),
            Self::X11(offer) => offer.disable(),
        }
    }
    fn drop(&self) {
        match self {
            Self::Wayland(offer) => offer.drop(),
            Self::X11(offer) => offer.drop(),
        }
    }
    fn validated(&self) -> bool {
        match self {
            Self::Wayland(offer) => offer.validated(),
            Self::X11(offer) => offer.validated(),
        }
    }
}

/// An offer always comes from the target that created it, so the variants
/// match; `None` means the target has no offer, such as a client-local drag.
fn wayland_offer<S: Source>(offer: Option<&mut Offer<S>>) -> Option<&mut WlOfferData<S>> {
    offer.and_then(|offer| match offer {
        Offer::Wayland(offer) => Some(offer),
        Offer::X11(_) => None,
    })
}

fn x11_offer<S: Source>(offer: Option<&mut Offer<S>>) -> Option<&mut XwmOfferData<S>> {
    offer.and_then(|offer| match offer {
        Offer::X11(offer) => Some(offer),
        Offer::Wayland(_) => None,
    })
}

impl DndFocus<State> for PointerFocus {
    type OfferData<S>
        = Offer<S>
    where
        S: Source;

    fn enter<S: Source>(
        &self,
        state: &mut State,
        dh: &DisplayHandle,
        source: Arc<S>,
        seat: &Seat<State>,
        location: Point<f64, Logical>,
        serial: &Serial,
    ) -> Option<Offer<S>> {
        match self {
            Self::Wayland(surface) => {
                DndFocus::enter(surface, state, dh, source, seat, location, serial)
                    .map(Offer::Wayland)
            }
            Self::X11(surface) => {
                DndFocus::enter(surface, state, dh, source, seat, location, serial).map(Offer::X11)
            }
        }
    }

    fn motion<S: Source>(
        &self,
        state: &mut State,
        offer: Option<&mut Offer<S>>,
        seat: &Seat<State>,
        location: Point<f64, Logical>,
        time: InputTime,
    ) {
        match self {
            Self::Wayland(surface) => {
                DndFocus::motion(surface, state, wayland_offer(offer), seat, location, time);
            }
            Self::X11(surface) => {
                DndFocus::motion(surface, state, x11_offer(offer), seat, location, time);
            }
        }
    }

    fn leave<S: Source>(
        &self,
        state: &mut State,
        offer: Option<&mut Offer<S>>,
        seat: &Seat<State>,
    ) {
        match self {
            Self::Wayland(surface) => DndFocus::leave(surface, state, wayland_offer(offer), seat),
            Self::X11(surface) => DndFocus::leave(surface, state, x11_offer(offer), seat),
        }
    }

    fn drop<S: Source>(&self, state: &mut State, offer: Option<&mut Offer<S>>, seat: &Seat<State>) {
        match self {
            Self::Wayland(surface) => DndFocus::drop(surface, state, wayland_offer(offer), seat),
            Self::X11(surface) => DndFocus::drop(surface, state, x11_offer(offer), seat),
        }
    }
}

impl WaylandDndGrabHandler for State {
    fn dnd_requested<S: Source>(
        &mut self,
        source: S,
        icon: Option<WlSurface>,
        seat: Seat<Self>,
        serial: Serial,
        grab: GrabType,
    ) {
        // The seat has no touch device, so every validated drag is a pointer drag.
        let (GrabType::Pointer, Some(pointer)) = (grab, seat.get_pointer()) else {
            source.cancel();
            return;
        };
        let Some(start) = pointer.grab_start_data() else {
            source.cancel();
            return;
        };
        self.drag = Some(Drag {
            icon,
            offset: (0, 0).into(),
        });
        pointer.set_grab(
            self,
            DnDGrab::new_pointer(&self.display_handle, start, source, seat.clone()),
            serial,
            Focus::Keep,
        );
        // The source loses pointer focus for the drag, so its cursor would
        // otherwise freeze on whatever it last set.
        self.cursor_image(&seat, CursorImageStatus::Named(CursorIcon::Grabbing));
        self.frame_pending = FrameRequest::Pending;
    }
}

// Smithay calls these while it holds the pointer's lock: touch only our state.
impl DndGrabHandler for State {
    fn dropped(
        &mut self,
        _: Option<DndTarget<'_, Self>>,
        _: bool,
        _: Seat<Self>,
        _: Point<f64, Logical>,
    ) {
        self.end_drag();
    }
    fn cancelled(&mut self, _: Seat<Self>, _: Point<f64, Logical>) {
        self.end_drag();
    }
}

impl State {
    fn end_drag(&mut self) {
        if self.drag.take().is_some() {
            self.frame_pending = FrameRequest::Pending;
        }
    }

    /// Whether a drag-and-drop grab holds the pointer. The compositor installs
    /// only window, click and popup grabs itself; any other grab is a drag,
    /// including X11 drags, whose source type Smithay does not export.
    pub(crate) fn dnd_grab_active(&self) -> bool {
        self.seat
            .get_pointer()
            .and_then(|pointer| {
                pointer.with_grab(|_, grab| {
                    !(grab.is::<crate::grabs::WindowGrab>()
                        || grab.is::<ClickGrab<Self>>()
                        || grab.is::<PopupPointerGrab<Self>>())
                })
            })
            .unwrap_or(false)
    }

    /// Cancel the active drag, if any: the source is told it was cancelled
    /// and no target receives a drop.
    pub(crate) fn cancel_drag(&mut self) {
        if self.dnd_grab_active()
            && let Some(pointer) = self.seat.get_pointer()
        {
            pointer.unset_grab(self, SERIAL_COUNTER.next_serial(), InputTime::now());
        }
    }

    /// A surface or X11 window was destroyed. Cancel a drag whose source
    /// surface is gone, and drop the icon if it was the icon.
    pub(crate) fn drag_surface_destroyed(&mut self, surface: Option<&WlSurface>) {
        if let Some(drag) = &mut self.drag
            && drag
                .icon
                .as_ref()
                .is_some_and(|icon| Some(icon) == surface || !smithay::utils::IsAlive::alive(icon))
        {
            drag.icon = None;
            self.frame_pending = FrameRequest::Pending;
        }
        if !self.dnd_grab_active() {
            return;
        }
        let orphaned = self
            .seat
            .get_pointer()
            .and_then(|pointer| pointer.grab_start_data())
            .and_then(|start| start.focus)
            .is_some_and(|(origin, _)| {
                !smithay::utils::IsAlive::alive(&origin) || surface.is_some_and(|s| origin.is(s))
            });
        if orphaned {
            self.cancel_drag();
        }
    }

    /// Re-pick the drop target at the pointer before a drop, so a target
    /// that disappeared since the last motion cannot accept it.
    pub(crate) fn refresh_drag_target(&mut self) {
        if !self.dnd_grab_active() {
            return;
        }
        let Some(pointer) = self.seat.get_pointer() else {
            return;
        };
        self.space.refresh();
        let location = pointer.current_location();
        pointer.motion(
            self,
            self.pointer_focus_under(location),
            &MotionEvent {
                location,
                serial: SERIAL_COUNTER.next_serial(),
                time: InputTime::now(),
            },
        );
    }
}
