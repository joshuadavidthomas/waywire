//! EI senders feed the same seat as browser input, with independent ownership.
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

use crate::compositor::State;

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
        // Device ids are unique within a connection, not across connections.
        let connection_id = KeyboardSource::new_auxiliary();
        let mut sources = HashMap::<String, KeyboardSource>::new();
        let layout = layout.clone();
        handle_clone
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
                            seat.add_pointer_absolute(
                                "absolute pointer",
                                &[EiRegion {
                                    rect,
                                    scale: state.output.current_scale().fractional_scale() as f32,
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
                            state.release_input(source);
                        }
                    }
                    EiInputEvent::Event(event) => match event {
                        InputEvent::DeviceAdded { device } => {
                            sources
                                .entry(device.id())
                                .or_insert_with(KeyboardSource::new_auxiliary);
                        }
                        InputEvent::DeviceRemoved { device } => {
                            if let Some(source) = sources.remove(&device.id()) {
                                state.release_input(source);
                            }
                        }
                        InputEvent::Keyboard { event } => {
                            let source = *sources
                                .entry(event.device().id())
                                .or_insert_with(KeyboardSource::new_auxiliary);
                            // Smithay's adapter converts evdev to XKB; our shared entry point accepts evdev.
                            state.keyboard_key(
                                source,
                                u32::from(event.key_code()) - 8,
                                event.state(),
                            );
                        }
                        InputEvent::PointerButton { event } => {
                            let source = *sources
                                .entry(event.device().id())
                                .or_insert_with(KeyboardSource::new_auxiliary);
                            state.pointer_button(source, event.button_code(), event.state());
                        }
                        InputEvent::PointerMotion { event } => {
                            if let Some(pointer) = state.seat.get_pointer() {
                                state.pointer_motion(pointer.current_location() + event.delta());
                            }
                        }
                        InputEvent::PointerMotionAbsolute { event } => {
                            // EI absolute coordinates are logical already, not normalized pixels.
                            state.pointer_motion((event.x(), event.y()).into());
                        }
                        InputEvent::PointerAxis { event } => {
                            if let Some(pointer) = state.seat.get_pointer() {
                                let mut frame = AxisFrame::new(event.time()).source(event.source());
                                for axis in [Axis::Horizontal, Axis::Vertical] {
                                    if let Some(v120) = event.amount_v120(axis) {
                                        frame = frame
                                            .v120(axis, v120 as i32)
                                            .value(axis, v120 * 15.0 / 120.0);
                                    } else if let Some(amount) = event.amount(axis) {
                                        frame = if amount == 0.0 {
                                            frame.stop(axis)
                                        } else {
                                            frame.value(axis, amount)
                                        };
                                    }
                                }
                                pointer.axis(state, frame);
                                pointer.frame(state);
                            }
                        }
                        _ => {}
                    },
                    // No text or touch device is advertised by this seat adapter.
                    EiInputEvent::TextKeysym { .. } | EiInputEvent::TextUtf8 { .. } => {}
                }
            })
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        Ok(PostAction::Continue)
    })?;
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
