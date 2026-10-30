//! The `zwlr_virtual_pointer_manager_v1` global, backed by Smithay's
//! `wayland::virtual_pointer` module.
//!
//! Smithay converts each request into an input-backend event. This compositor
//! dispatches input directly instead (see `eis.rs`), so the handler below sends
//! those events to the same seat entry points the browser's input uses. Focus,
//! decorations and cursor events then behave exactly like browser input.
//!
//! Control-ownership arbitration for this input source is planned in
//! ROADMAP.md (pass 1, input arbitration) and is not part of this change:
//! virtual-pointer input bypasses the browser's control lease today, exactly
//! like EIS input.
use smithay::backend::input::AbsolutePositionEvent;
use smithay::backend::input::Axis;
use smithay::backend::input::Event;
use smithay::backend::input::InputEvent;
use smithay::backend::input::PointerAxisEvent;
use smithay::backend::input::PointerButtonEvent;
use smithay::backend::input::PointerMotionEvent;
use smithay::input::keyboard::KeyboardSource;
use smithay::input::pointer::AxisFrame;
use smithay::reexports::wayland_server::DisplayHandle;
use smithay::wayland::virtual_pointer::VirtualPointerBackend;
use smithay::wayland::virtual_pointer::VirtualPointerHandler;
use smithay::wayland::virtual_pointer::VirtualPointerManagerState;

use crate::State;

/// Register the virtual pointer manager global.
pub(crate) fn install(display_handle: &DisplayHandle) {
    // One trusted desktop session: local clients that can reach the socket may
    // create a virtual pointer without further screening.
    VirtualPointerManagerState::new::<State, _>(display_handle, |_client| true);
}

impl VirtualPointerHandler for State {
    fn process_virtual_pointer_event(&mut self, event: InputEvent<VirtualPointerBackend>) {
        match event {
            InputEvent::DeviceAdded { device } => {
                // One source per virtual pointer, like one per EI device, so its
                // held buttons are tracked and released independently of other
                // input sources.
                self.virtual_pointers
                    .insert(device, KeyboardSource::new_auxiliary());
            }
            InputEvent::DeviceRemoved { device } => {
                // A client may destroy the pointer, or die, with buttons still
                // down. Releasing the source's input clears Close arming, held
                // buttons and keyboard state together, like an unplugged mouse.
                if let Some(source) = self.virtual_pointers.remove(&device) {
                    self.release_input(source);
                }
            }
            InputEvent::PointerMotion { event } => {
                if let Some(pointer) = self.seat.get_pointer() {
                    let delta = event.delta();
                    self.pointer_motion_with_delta(pointer.current_location() + delta, Some(delta));
                }
            }
            InputEvent::PointerMotionAbsolute { event } => {
                // The event is a fraction of the client's extents, already
                // clamped to 0..=1. Map it onto the output's logical size.
                let scale = self.output.current_scale().fractional_scale();
                self.pointer_motion(
                    (
                        absolute_position(event.x(), self.size.width(), scale),
                        absolute_position(event.y(), self.size.height(), scale),
                    )
                        .into(),
                );
            }
            InputEvent::PointerButton { event } => {
                let source = *self
                    .virtual_pointers
                    .entry(event.device())
                    .or_insert_with(KeyboardSource::new_auxiliary);
                self.pointer_button(source, event.button_code(), event.state());
            }
            InputEvent::PointerAxis { event } => {
                if let Some(pointer) = self.seat.get_pointer() {
                    // One frame per `frame` request, timestamped with the
                    // scroll event's own time.
                    let mut frame = AxisFrame::new(event.time()).source(event.source());
                    for axis in [Axis::Horizontal, Axis::Vertical] {
                        match axis_event(event.amount(axis), event.amount_v120(axis)) {
                            Some(AxisEvent::Value(value)) => {
                                frame = frame.value(axis, value);
                            }
                            Some(AxisEvent::Steps { value, v120 }) => {
                                frame = frame.v120(axis, v120).value(axis, value);
                            }
                            Some(AxisEvent::Stop) => {
                                frame = frame.stop(axis);
                            }
                            None => {}
                        }
                    }
                    pointer.axis(self, frame);
                    pointer.frame(self);
                }
            }
            // This backend only produces pointer events.
            InputEvent::Keyboard { .. }
            | InputEvent::GestureSwipeBegin { .. }
            | InputEvent::GestureSwipeUpdate { .. }
            | InputEvent::GestureSwipeEnd { .. }
            | InputEvent::GesturePinchBegin { .. }
            | InputEvent::GesturePinchUpdate { .. }
            | InputEvent::GesturePinchEnd { .. }
            | InputEvent::GestureHoldBegin { .. }
            | InputEvent::GestureHoldEnd { .. }
            | InputEvent::TouchDown { .. }
            | InputEvent::TouchMotion { .. }
            | InputEvent::TouchUp { .. }
            | InputEvent::TouchCancel { .. }
            | InputEvent::TouchFrame { .. }
            | InputEvent::TabletToolAxis { .. }
            | InputEvent::TabletToolProximity { .. }
            | InputEvent::TabletToolTip { .. }
            | InputEvent::TabletToolButton { .. }
            | InputEvent::SwitchToggle { .. }
            | InputEvent::Special(_) => {}
        }
    }
}

/// The client-visible scroll value for one axis of a frame.
#[derive(Debug, Clone, Copy)]
enum AxisEvent {
    /// A smooth value in logical pixels.
    Value(f64),
    /// Discrete steps, as a v120 step count and its logical pixel value.
    Steps { value: f64, v120: i32 },
    /// The axis stopped.
    Stop,
}

/// Converts one axis of an upstream scroll event into the value sent to
/// clients, following the EI adapter's mapping: discrete steps in v120 units
/// carry a value of steps times 15/120 logical pixels.
fn axis_event(amount: Option<f64>, v120: Option<f64>) -> Option<AxisEvent> {
    if let Some(v120) = v120 {
        #[allow(clippy::cast_possible_truncation)] // Wheel notch counts are small integers.
        let v120 = v120 as i32;
        Some(AxisEvent::Steps {
            value: f64::from(v120) * 15.0 / 120.0,
            v120,
        })
    } else {
        amount.map(|amount| {
            if amount == 0.0 {
                AxisEvent::Stop
            } else {
                AxisEvent::Value(amount)
            }
        })
    }
}

/// Maps a normalized virtual-pointer position onto the output's logical size.
fn absolute_position(fraction: f64, output_pixels: u32, scale: f64) -> f64 {
    fraction * f64::from(output_pixels) / scale
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
        assert_eq!(absolute_position(0.0, 1920, 1.0), 0.0);
        assert_eq!(absolute_position(0.5, 1920, 1.0), 960.0);
        assert_eq!(absolute_position(1.0, 1920, 1.0), 1920.0);
        // A fractional output scale divides the mapped pixels.
        assert_eq!(absolute_position(0.5, 1920, 2.0), 480.0);
    }

    #[test]
    fn axis_events_map_steps_and_stops() {
        assert!(matches!(axis_event(Some(0.0), None), Some(AxisEvent::Stop)));
        assert!(matches!(
            axis_event(Some(-7.5), None),
            Some(AxisEvent::Value(-7.5))
        ));
        // One wheel notch is 120 v120 units and 15 logical pixels.
        assert!(matches!(
            axis_event(Some(0.0), Some(120.0)),
            Some(AxisEvent::Steps {
                value: 15.0,
                v120: 120
            })
        ));
        assert!(axis_event(None, None).is_none());
    }
}
