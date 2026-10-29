//! The `zwlr_virtual_pointer_manager_v1` global, ported from Smithay's upstream
//! `wayland::virtual_pointer` module (commit `2bf8be86`) that niri's compositor
//! routes into its input pipeline. Smithay at our pinned revision (`d33a9c98`)
//! does not ship that module yet, so the protocol handling lives here.
//!
//! Niri converts each request into a backend input event and feeds it through
//! its input backend. This compositor dispatches input directly instead (see
//! `eis.rs`), so the requests below go straight to the same seat entry points
//! the browser's input uses, and focus, decorations and cursor events behave
//! exactly like browser input.
//!
//! Control-ownership arbitration for this input source is planned in
//! ROADMAP.md (pass 1, input arbitration) and is not part of this change:
//! virtual-pointer input bypasses the browser's control lease today, exactly
//! like EIS input.
use std::sync::Mutex;
use std::sync::MutexGuard;
use std::sync::PoisonError;

use smithay::backend::input::Axis;
use smithay::backend::input::AxisSource;
use smithay::backend::input::ButtonState;
use smithay::backend::input::InputTime;
use smithay::input::keyboard::KeyboardSource;
use smithay::input::pointer::AxisFrame;
use smithay::reexports::wayland_protocols_wlr::virtual_pointer::v1::server::zwlr_virtual_pointer_manager_v1;
use smithay::reexports::wayland_protocols_wlr::virtual_pointer::v1::server::zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1;
use smithay::reexports::wayland_protocols_wlr::virtual_pointer::v1::server::zwlr_virtual_pointer_v1;
use smithay::reexports::wayland_protocols_wlr::virtual_pointer::v1::server::zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1;
use smithay::reexports::wayland_server::Client;
use smithay::reexports::wayland_server::DataInit;
use smithay::reexports::wayland_server::DisplayHandle;
use smithay::reexports::wayland_server::New;
use smithay::reexports::wayland_server::Resource;
use smithay::reexports::wayland_server::WEnum;
use smithay::reexports::wayland_server::backend::ClientId;
use smithay::reexports::wayland_server::protocol::wl_pointer;
use smithay::utils::Logical;
use smithay::utils::Point;
use smithay::wayland::Dispatch2;
use smithay::wayland::GlobalData;
use smithay::wayland::GlobalDispatch2;

use crate::State;

/// Advertise the virtual-pointer manager at protocol version 2. Like the
/// seat's other globals, every client of the socket may bind it.
pub(crate) fn install(display_handle: &DisplayHandle) {
    display_handle
        .create_global::<State, ZwlrVirtualPointerManagerV1, _>(MANAGER_VERSION, GlobalData);
}

/// `zwlr_virtual_pointer_v1` is stable at version 2.
const MANAGER_VERSION: u32 = 2;

impl GlobalDispatch2<ZwlrVirtualPointerManagerV1, State> for GlobalData {
    fn bind(
        &self,
        _: &mut State,
        _: &DisplayHandle,
        _: &Client,
        resource: New<ZwlrVirtualPointerManagerV1>,
        data_init: &mut DataInit<'_, State>,
    ) {
        data_init.init(resource, GlobalData);
    }
}

#[allow(
    clippy::wildcard_enum_match_arm,
    reason = "wayland-generated request enums are closed; unknown variants cannot occur at this protocol version"
)]
impl Dispatch2<ZwlrVirtualPointerManagerV1, State> for GlobalData {
    fn request(
        &self,
        _state: &mut State,
        _client: &Client,
        _resource: &ZwlrVirtualPointerManagerV1,
        request: zwlr_virtual_pointer_manager_v1::Request,
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, State>,
    ) {
        // The seat and output arguments are ignored: this compositor has one
        // seat and one output, which the input entry points already target.
        let id = match request {
            zwlr_virtual_pointer_manager_v1::Request::CreateVirtualPointer { id, .. }
            | zwlr_virtual_pointer_manager_v1::Request::CreateVirtualPointerWithOutput {
                id, ..
            } => id,
            // Destroy needs no response, and no other variants exist at this
            // protocol version.
            zwlr_virtual_pointer_manager_v1::Request::Destroy | _ => return,
        };
        let data = VirtualPointerUserData {
            // One source per virtual pointer, like one per EI device, so its
            // held buttons are tracked and released independently of other
            // input sources.
            source: KeyboardSource::new_auxiliary(),
            frame: Mutex::new(PendingFrame::default()),
        };
        data_init.init(id, data);
    }
}

/// User data for a `zwlr_virtual_pointer_v1`.
struct VirtualPointerUserData {
    /// Input source identifying this pointer's events and held buttons.
    source: KeyboardSource,
    /// Scroll requests buffered between two `frame` requests.
    frame: Mutex<PendingFrame>,
}

/// Locks the pending scroll frame, recovering from a poisoned lock: the data
/// is a plain accumulator a panicked handler leaves usable.
fn pending(frame: &Mutex<PendingFrame>) -> MutexGuard<'_, PendingFrame> {
    frame.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Scroll state accumulated for one axis between two `frame` requests.
#[derive(Debug, Default, Clone, Copy)]
struct PendingAxis {
    value: f64,
    /// Discrete steps in v120 units, when the client sent an `axis_discrete`
    /// request for this axis. `None` when it only sent `axis`.
    v120: Option<i32>,
}

/// Scroll state accumulated between two `frame` requests.
#[derive(Debug, Default)]
struct PendingFrame {
    source: Option<AxisSource>,
    /// Pending state for the horizontal and vertical axes.
    axes: [Option<PendingAxis>; 2],
}

fn axis_index(axis: Axis) -> usize {
    match axis {
        Axis::Horizontal => 0,
        Axis::Vertical => 1,
    }
}

fn parse_axis(
    virtual_pointer: &ZwlrVirtualPointerV1,
    axis: WEnum<wl_pointer::Axis>,
) -> Option<Axis> {
    match axis {
        WEnum::Value(wl_pointer::Axis::HorizontalScroll) => Some(Axis::Horizontal),
        WEnum::Value(wl_pointer::Axis::VerticalScroll) => Some(Axis::Vertical),
        WEnum::Value(_) | WEnum::Unknown(_) => {
            virtual_pointer.post_error(zwlr_virtual_pointer_v1::Error::InvalidAxis, "invalid axis");
            None
        }
    }
}

#[allow(
    clippy::wildcard_enum_match_arm,
    reason = "wayland-generated request enums are closed; unknown variants cannot occur at this protocol version"
)]
impl Dispatch2<ZwlrVirtualPointerV1, State> for VirtualPointerUserData {
    fn request(
        &self,
        state: &mut State,
        _client: &Client,
        virtual_pointer: &ZwlrVirtualPointerV1,
        request: zwlr_virtual_pointer_v1::Request,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, State>,
    ) {
        match request {
            zwlr_virtual_pointer_v1::Request::Motion { dx, dy, .. } => {
                let Some(pointer) = state.seat.get_pointer() else {
                    return;
                };
                let delta: Point<f64, Logical> = (dx, dy).into();
                state.pointer_motion_with_delta(pointer.current_location() + delta, Some(delta));
            }
            zwlr_virtual_pointer_v1::Request::MotionAbsolute {
                x,
                y,
                x_extent,
                y_extent,
                ..
            } => {
                if x_extent == 0 || y_extent == 0 {
                    tracing::debug!("dropping virtual pointer absolute motion with zero extents");
                    return;
                }
                let scale = state.output.current_scale().fractional_scale();
                let location: Point<f64, Logical> = (
                    absolute_position(x, x_extent, state.size.width(), scale),
                    absolute_position(y, y_extent, state.size.height(), scale),
                )
                    .into();
                state.pointer_motion(location);
            }
            zwlr_virtual_pointer_v1::Request::Button {
                button,
                state: button_state,
                ..
            } => {
                // The protocol defines this as an enum, but wlroots treats it
                // as a C boolean (zero or nonzero), so any invalid value counts
                // as pressed.
                let button_state = match button_state {
                    WEnum::Value(wl_pointer::ButtonState::Released) => ButtonState::Released,
                    // wlroots treats invalid values as a C boolean, so press.
                    WEnum::Value(_) | WEnum::Unknown(_) => ButtonState::Pressed,
                };
                state.pointer_button(self.source, button, button_state);
            }
            zwlr_virtual_pointer_v1::Request::Axis { axis, value, .. } => {
                let Some(axis) = parse_axis(virtual_pointer, axis) else {
                    return;
                };
                let mut frame = pending(&self.frame);
                let pending_axis = frame.axes[axis_index(axis)].get_or_insert_default();
                pending_axis.value = value;
            }
            zwlr_virtual_pointer_v1::Request::AxisSource { axis_source } => {
                let axis_source = match axis_source {
                    WEnum::Value(wl_pointer::AxisSource::Wheel) => AxisSource::Wheel,
                    WEnum::Value(wl_pointer::AxisSource::Finger) => AxisSource::Finger,
                    WEnum::Value(wl_pointer::AxisSource::Continuous) => AxisSource::Continuous,
                    WEnum::Value(wl_pointer::AxisSource::WheelTilt) => AxisSource::WheelTilt,
                    WEnum::Value(_) | WEnum::Unknown(_) => {
                        virtual_pointer.post_error(
                            zwlr_virtual_pointer_v1::Error::InvalidAxisSource,
                            "invalid axis source",
                        );
                        return;
                    }
                };
                pending(&self.frame).source = Some(axis_source);
            }
            zwlr_virtual_pointer_v1::Request::AxisStop { axis, .. } => {
                let Some(axis) = parse_axis(virtual_pointer, axis) else {
                    return;
                };
                let mut frame = pending(&self.frame);
                // A zero value without any discrete steps is a stop.
                frame.axes[axis_index(axis)] = Some(PendingAxis::default());
            }
            zwlr_virtual_pointer_v1::Request::AxisDiscrete {
                axis,
                value,
                discrete,
                ..
            } => {
                let Some(axis) = parse_axis(virtual_pointer, axis) else {
                    return;
                };
                let mut frame = pending(&self.frame);
                let pending_axis = frame.axes[axis_index(axis)].get_or_insert_default();
                pending_axis.value = value;
                pending_axis.v120 = Some(discrete.saturating_mul(120));
            }
            zwlr_virtual_pointer_v1::Request::Frame => {
                let frame = std::mem::take(&mut *pending(&self.frame));
                if !frame.axes.iter().any(Option::is_some) {
                    return;
                }
                if let Some(pointer) = state.seat.get_pointer() {
                    pointer.axis(state, axis_frame(&frame));
                    pointer.frame(state);
                }
            }
            // Destroy is a no-op: the pointer's input is released in
            // `destroyed`. No other variants exist at this protocol version.
            zwlr_virtual_pointer_v1::Request::Destroy | _ => {}
        }
    }

    fn destroyed(
        &self,
        state: &mut State,
        _client: ClientId,
        _virtual_pointer: &ZwlrVirtualPointerV1,
    ) {
        // A client may destroy the pointer, or die, with buttons still down.
        // Releasing the source's input clears Close arming, held buttons and
        // keyboard state together, like an unplugged mouse.
        state.release_input(self.source);
    }
}

/// The client-visible scroll value for one axis of a frame.
enum AxisEvent {
    /// A smooth value in logical pixels.
    Value(f64),
    /// Discrete steps, as a v120 step count and its logical pixel value.
    Steps { value: f64, v120: i32 },
    /// The axis stopped.
    Stop,
}

/// Converts the scroll requests a client sent since its last `frame` into the
/// event for one axis, following the EI adapter's value mapping: discrete
/// steps in v120 units carry a value of steps times 15/120 logical pixels.
fn axis_event(pending_axis: PendingAxis) -> AxisEvent {
    if let Some(v120) = pending_axis.v120 {
        AxisEvent::Steps {
            value: f64::from(v120) * 15.0 / 120.0,
            v120,
        }
    } else if pending_axis.value == 0.0 {
        AxisEvent::Stop
    } else {
        AxisEvent::Value(pending_axis.value)
    }
}

/// Builds one Smithay axis frame from a completed pending frame.
fn axis_frame(frame: &PendingFrame) -> AxisFrame {
    // wlroots defaults to wheel when the client doesn't send `axis_source`.
    let mut axis_frame =
        AxisFrame::new(InputTime::now()).source(frame.source.unwrap_or(AxisSource::Wheel));
    for (axis, pending_axis) in [
        (Axis::Horizontal, frame.axes[0]),
        (Axis::Vertical, frame.axes[1]),
    ] {
        match pending_axis.map(axis_event) {
            Some(AxisEvent::Value(value)) => {
                axis_frame = axis_frame.value(axis, value);
            }
            Some(AxisEvent::Steps { value, v120 }) => {
                axis_frame = axis_frame.v120(axis, v120).value(axis, value);
            }
            Some(AxisEvent::Stop) => {
                axis_frame = axis_frame.stop(axis);
            }
            None => {}
        }
    }
    axis_frame
}

/// Maps a virtual-pointer absolute position onto the output.
///
/// The protocol defines the position as a fraction of client-chosen extents.
/// Clients can send positions outside the extents; clamp like Smithay's own
/// event mapping before scaling onto the output's logical size, its pixels
/// divided by the fractional scale.
fn absolute_position(position: u32, extent: u32, output_pixels: u32, scale: f64) -> f64 {
    f64::from(position.min(extent)) / f64::from(extent) * f64::from(output_pixels) / scale
}

#[cfg(test)]
#[expect(
    clippy::float_cmp,
    reason = "the extent mapping is exact for the tested values"
)]
mod tests {
    use super::*;

    #[test]
    fn absolute_extents_map_onto_the_output() {
        assert_eq!(absolute_position(0, 100, 1920, 1.0), 0.0);
        assert_eq!(absolute_position(50, 100, 1920, 1.0), 960.0);
        assert_eq!(absolute_position(100, 100, 1920, 1.0), 1920.0);
        // Positions past the extent clamp to its edge.
        assert_eq!(absolute_position(250, 100, 1920, 1.0), 1920.0);
        // A fractional output scale divides the mapped pixels.
        assert_eq!(absolute_position(50, 100, 1920, 2.0), 480.0);
    }

    #[test]
    fn axis_events_map_steps_and_stops() {
        assert!(matches!(
            axis_event(PendingAxis {
                value: 0.0,
                v120: None,
            }),
            AxisEvent::Stop
        ));
        assert!(matches!(
            axis_event(PendingAxis {
                value: -7.5,
                v120: None,
            }),
            AxisEvent::Value(-7.5)
        ));
        // One wheel notch is 120 v120 units and 15 logical pixels.
        assert!(matches!(
            axis_event(PendingAxis {
                value: 0.0,
                v120: Some(120),
            }),
            AxisEvent::Steps {
                value: 15.0,
                v120: 120
            }
        ));
    }
}
