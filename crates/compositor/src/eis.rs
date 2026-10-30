//! EI senders feed the same seat as browser input, under one ownership model.
use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;
use smithay::backend::input::AbsolutePositionEvent;
use smithay::backend::input::Axis;
use smithay::backend::input::Device;
use smithay::backend::input::Event;
use smithay::backend::input::InputEvent;
use smithay::backend::input::KeyboardKeyEvent;
use smithay::backend::input::PointerAxisEvent;
use smithay::backend::input::PointerButtonEvent;
use smithay::backend::input::PointerMotionEvent;
use smithay::backend::libei::EiInput;
use smithay::backend::libei::EiInputEvent;
use smithay::backend::libei::EiRegion;
use smithay::input::keyboard::KeyboardSource;
use smithay::input::keyboard::XkbConfig;
use smithay::input::pointer::AxisFrame;
use smithay::reexports::calloop::LoopHandle;
use smithay::reexports::calloop::PostAction;
use smithay::reexports::reis::calloop::EisListenerSource;
use smithay::reexports::reis::eis;

use crate::State;

// Keep the adapter's lifecycle and event dispatch together. EI scale is f32,
// and its discrete scroll is an i32 exposed as f64 by the generic input trait.
#[allow(
    clippy::too_many_lines,
    clippy::cast_possible_truncation,
    clippy::wildcard_enum_match_arm
)]
pub(crate) fn listen(
    handle: &LoopHandle<'static, State>,
    path: &Path,
    layout: String,
) -> Result<()> {
    let listener = eis::Listener::bind(path)?;
    let handle_clone = handle.clone();
    handle.insert_source(EisListenerSource::new(listener), move |context, (), _| {
        serve_connection(&handle_clone, context, layout.clone())
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        Ok(PostAction::Continue)
    })?;
    Ok(())
}

/// Serve one EI connection, whether accepted from the listening socket or
/// handed out over D-Bus by the `RemoteDesktop` portal's `ConnectToEIS`. Both
/// feed the same seat as browser input, and every device is an agent source
/// that a browser takeover releases and then bypasses.
#[expect(
    clippy::too_many_lines,
    reason = "one function owns the EI event pump for a single connection"
)]
pub(crate) fn serve_connection(
    handle: &LoopHandle<'static, State>,
    context: eis::Context,
    layout: String,
) -> Result<()> {
    // Device ids are unique within a connection, not across connections.
    let connection_id = KeyboardSource::new_auxiliary();
    let mut sources = HashMap::<String, KeyboardSource>::new();
    handle
        .insert_source(EiInput::new(context), move |event, connection, state| {
            match event {
                EiInputEvent::Connected => {
                    tracing::debug!("EI sender connected");
                    let seat = connection.add_seat("waywire");
                    if let Err(error) = seat.add_keyboard(
                        "keyboard",
                        XkbConfig {
                            layout: &layout,
                            ..Default::default()
                        },
                    ) {
                        tracing::error!(%error, "EI keyboard creation failed");
                    }
                    seat.add_pointer("pointer");
                    if let Some(rect) = state.space.output_geometry(&state.output) {
                        #[allow(clippy::cast_possible_truncation)]
                        // EI represents output scale as f32.
                        let scale = state.output.current_scale().fractional_scale() as f32;
                        seat.add_pointer_absolute(
                            "absolute pointer",
                            &[EiRegion {
                                rect,
                                scale,
                                mapping_id: Some(state.output.name()),
                            }],
                        );
                    }
                    state.eis_seats.insert(connection_id, seat);
                }
                EiInputEvent::Disconnected => {
                    tracing::debug!("EI sender disconnected");
                    state.eis_seats.remove(&connection_id);
                    for (_, source) in sources.drain() {
                        state.disconnect_input_source(source);
                    }
                }
                EiInputEvent::Event(event) => match event {
                    InputEvent::DeviceAdded { device } => {
                        let source = *sources
                            .entry(device.id())
                            .or_insert_with(KeyboardSource::new_auxiliary);
                        state.register_agent_source(source);
                    }
                    InputEvent::DeviceRemoved { device } => {
                        if let Some(source) = sources.remove(&device.id()) {
                            state.disconnect_input_source(source);
                        }
                    }
                    InputEvent::Keyboard { event } => {
                        let source = *sources
                            .entry(event.device().id())
                            .or_insert_with(KeyboardSource::new_auxiliary);
                        // Smithay's adapter converts evdev to XKB; our shared entry point accepts evdev.
                        // Evdev key codes below KEY_ESC (9) are not key events;
                        // subtracting the 8-code XKB offset would underflow.
                        let Some(key) = u32::from(event.key_code()).checked_sub(8) else {
                            tracing::debug!(
                                key_code = u32::from(event.key_code()),
                                "dropped EI key code below the evdev key range"
                            );
                            return;
                        };
                        state.keyboard_key(source, key, event.state());
                    }
                    InputEvent::PointerButton { event } => {
                        let source = *sources
                            .entry(event.device().id())
                            .or_insert_with(KeyboardSource::new_auxiliary);
                        state.pointer_button(source, event.button_code(), event.state());
                    }
                    InputEvent::PointerMotion { event } => {
                        let source = *sources
                            .entry(event.device().id())
                            .or_insert_with(KeyboardSource::new_auxiliary);
                        state.register_agent_source(source);
                        if let Some(pointer) = state.seat.get_pointer() {
                            state.pointer_motion_with_delta(
                                source,
                                pointer.current_location() + event.delta(),
                                Some(event.delta()),
                            );
                        }
                    }
                    InputEvent::PointerMotionAbsolute { event } => {
                        let source = *sources
                            .entry(event.device().id())
                            .or_insert_with(KeyboardSource::new_auxiliary);
                        state.register_agent_source(source);
                        // EI absolute coordinates are logical already, not normalized pixels.
                        state.pointer_motion(source, (event.x(), event.y()).into());
                    }
                    InputEvent::PointerAxis { event } => {
                        let source = *sources
                            .entry(event.device().id())
                            .or_insert_with(KeyboardSource::new_auxiliary);
                        state.register_agent_source(source);
                        let mut frame = AxisFrame::new(event.time()).source(event.source());
                        for axis in [Axis::Horizontal, Axis::Vertical] {
                            if let Some(v120) = event.amount_v120(axis) {
                                #[allow(clippy::cast_possible_truncation)]
                                // Wheel notch counts are small integers in v120 units.
                                let (notches, scroll) = (v120 as i32, v120 * 15.0 / 120.0);
                                frame = frame.v120(axis, notches).value(axis, scroll);
                            } else if let Some(amount) = event.amount(axis) {
                                frame = if amount == 0.0 {
                                    frame.stop(axis)
                                } else {
                                    frame.value(axis, amount)
                                };
                            }
                        }
                        state.pointer_axis(source, frame);
                    }
                    // Any other event kind this seat adapter does not produce.
                    InputEvent::GestureSwipeBegin { .. }
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
                },
            }
        })
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    Ok(())
}

/// EI regions are immutable per device. Smithay replaces the absolute devices,
/// emits `DeviceRemoved` (releasing only their held input), then advertises the new
/// regions. Relative pointers and keyboards remain live across output changes.
#[allow(clippy::cast_possible_truncation)] // EI represents output scale as f32.
pub(crate) fn refresh_regions(state: &State) {
    if let Some(rect) = state.space.output_geometry(&state.output) {
        let regions = [EiRegion {
            rect,
            scale: state.output.current_scale().fractional_scale() as f32,
            mapping_id: Some(state.output.name()),
        }];
        for seat in state.eis_seats.values() {
            seat.update_regions(&regions);
        }
    }
}
