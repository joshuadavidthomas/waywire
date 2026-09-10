use std::fs::File;
use std::io::Write;
use std::os::fd::AsFd;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use nix::sys::memfd::MFdFlags;
use nix::sys::memfd::memfd_create;
use nix::unistd::ftruncate;
use sprite_desktop_protocol::pipe::ButtonState;
use sprite_desktop_protocol::pipe::Command;
use sprite_desktop_protocol::pipe::KeyCode;
use sprite_desktop_protocol::pipe::KeyState;
use sprite_desktop_protocol::pipe::PointerButton;
use sprite_desktop_protocol::pipe::TextAction;
use wayland_client::Proxy;
use wayland_client::QueueHandle;
use wayland_client::protocol::wl_keyboard;
use wayland_client::protocol::wl_output;
use wayland_client::protocol::wl_pointer;
use wayland_client::protocol::wl_seat;
use wayland_protocols_misc::zwp_input_method_v2::client::zwp_input_method_manager_v2;
use wayland_protocols_misc::zwp_input_method_v2::client::zwp_input_method_v2;
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::zwp_virtual_keyboard_manager_v1;
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::zwp_virtual_keyboard_v1;
use wayland_protocols_wlr::virtual_pointer::v1::client::zwlr_virtual_pointer_manager_v1;
use wayland_protocols_wlr::virtual_pointer::v1::client::zwlr_virtual_pointer_v1;
use xkbcommon::xkb;

use super::State;

pub(crate) struct Input {
    pub(crate) seat: Option<wl_seat::WlSeat>,
    pub(crate) pointer_manager:
        Option<zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1>,
    pub(crate) keyboard_manager:
        Option<zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1>,
    pub(crate) method_manager: Option<zwp_input_method_manager_v2::ZwpInputMethodManagerV2>,
    pointer: Option<zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1>,
    keyboard: Option<zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1>,
    pub(crate) method: Option<zwp_input_method_v2::ZwpInputMethodV2>,
    method_active: bool,
    pending_method_active: Option<bool>,
    method_serial: u32,
    xkb_state: Option<xkb::State>,
    pressed_keys: [bool; 256],
    pressed_buttons: [bool; 5],
    layout: String,
}

impl Input {
    pub(crate) fn new(layout: String) -> Self {
        Self {
            seat: None,
            pointer_manager: None,
            keyboard_manager: None,
            method_manager: None,
            pointer: None,
            keyboard: None,
            method: None,
            method_active: false,
            pending_method_active: None,
            method_serial: 0,
            xkb_state: None,
            pressed_keys: [false; 256],
            pressed_buttons: [false; 5],
            layout,
        }
    }

    pub(crate) fn start(
        &mut self,
        output: &wl_output::WlOutput,
        qh: &QueueHandle<State>,
    ) -> Result<()> {
        let seat = self.seat.clone().context("missing wl_seat")?;
        let pointer_manager = self
            .pointer_manager
            .as_ref()
            .context("missing virtual pointer manager")?;
        let keyboard_manager = self
            .keyboard_manager
            .as_ref()
            .context("missing virtual keyboard manager")?;
        self.pointer = Some(if pointer_manager.version() >= 2 {
            pointer_manager.create_virtual_pointer_with_output(Some(&seat), Some(output), qh, ())
        } else {
            pointer_manager.create_virtual_pointer(Some(&seat), qh, ())
        });
        let keyboard = keyboard_manager.create_virtual_keyboard(&seat, qh, ());
        self.install_keymap(&keyboard)?;
        self.keyboard = Some(keyboard);
        if let Some(manager) = &self.method_manager {
            self.method = Some(manager.get_input_method(&seat, qh, ()));
        }
        Ok(())
    }

    fn install_keymap(
        &mut self,
        keyboard: &zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1,
    ) -> Result<()> {
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let keymap = xkb::Keymap::new_from_names(
            &context,
            "",
            "",
            &self.layout,
            "",
            None,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
        .context("xkbcommon rejected the keyboard layout")?;
        let mut text = keymap
            .get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1)
            .into_bytes();
        text.push(0);
        let fd = memfd_create(c"sprite-desktop-keymap", MFdFlags::MFD_CLOEXEC)?;
        ftruncate(&fd, i64::try_from(text.len())?)?;
        let mut file = File::from(fd);
        file.write_all(&text)?;
        keyboard.keymap(
            wl_keyboard::KeymapFormat::XkbV1 as u32,
            file.as_fd(),
            u32::try_from(text.len())?,
        );
        self.xkb_state = Some(xkb::State::new(&keymap));
        Ok(())
    }

    pub(crate) fn apply(&mut self, command: &Command) -> Result<()> {
        let time = monotonic_millis()?;
        match command {
            Command::PointerAbsolute {
                x: horizontal,
                y: vertical,
                ..
            } => {
                let pointer = self
                    .pointer
                    .as_ref()
                    .context("virtual pointer unavailable")?;
                pointer.motion_absolute(time, horizontal.get(), vertical.get(), 65_535, 65_535);
                pointer.frame();
            }
            Command::PointerRelative { dx, dy, .. } => {
                let pointer = self
                    .pointer
                    .as_ref()
                    .context("virtual pointer unavailable")?;
                pointer.motion(time, f64::from(dx.get()), f64::from(dy.get()));
                pointer.frame();
            }
            Command::PointerButton { button, state, .. } => {
                self.button(time, *button, *state)?;
            }
            Command::PointerScroll { dx, dy, .. } => {
                let pointer = self
                    .pointer
                    .as_ref()
                    .context("virtual pointer unavailable")?;
                pointer.axis_source(wl_pointer::AxisSource::Continuous);
                if dx.get() != 0.0 {
                    pointer.axis(
                        time,
                        wl_pointer::Axis::HorizontalScroll,
                        f64::from(dx.get()),
                    );
                }
                if dy.get() != 0.0 {
                    pointer.axis(time, wl_pointer::Axis::VerticalScroll, f64::from(dy.get()));
                }
                pointer.frame();
            }
            Command::KeyboardKey { key, state, .. } => self.key(time, *key, *state)?,
            Command::ReleaseAll => self.release_all()?,
            Command::Text { action, text, .. } => {
                self.send_text(*action, text.as_str())?;
            }
            Command::Resize { .. }
            | Command::Clipboard(_)
            | Command::Quality { .. }
            | Command::KeyframeReadiness { .. } => {
                bail!("non-input command passed to input module")
            }
        }
        Ok(())
    }

    fn button(&mut self, time: u32, button: PointerButton, state: ButtonState) -> Result<()> {
        let index = button_index(button);
        let pressed = state == ButtonState::Pressed;
        if self.pressed_buttons[index] == pressed {
            return Ok(());
        }
        self.pressed_buttons[index] = pressed;
        let pointer = self
            .pointer
            .as_ref()
            .context("virtual pointer unavailable")?;
        pointer.button(
            time,
            button.evdev_code(),
            match state {
                ButtonState::Released => wl_pointer::ButtonState::Released,
                ButtonState::Pressed => wl_pointer::ButtonState::Pressed,
            },
        );
        pointer.frame();
        Ok(())
    }

    fn key(&mut self, time: u32, key: KeyCode, state: KeyState) -> Result<()> {
        let index = usize::try_from(key.get())?;
        let keyboard = self
            .keyboard
            .as_ref()
            .context("virtual keyboard unavailable")?;
        if state == KeyState::Repeated {
            if self.pressed_keys[index] {
                keyboard.key(time, key.get(), wl_keyboard::KeyState::Released as u32);
                keyboard.key(time, key.get(), wl_keyboard::KeyState::Pressed as u32);
            }
            return Ok(());
        }
        let pressed = state == KeyState::Pressed;
        if self.pressed_keys[index] == pressed {
            return Ok(());
        }
        self.pressed_keys[index] = pressed;
        keyboard.key(
            time,
            key.get(),
            if pressed {
                wl_keyboard::KeyState::Pressed
            } else {
                wl_keyboard::KeyState::Released
            } as u32,
        );
        if let Some(xkb_state) = &mut self.xkb_state {
            xkb_state.update_key(
                xkb::Keycode::new(key.get() + 8),
                if pressed {
                    xkb::KeyDirection::Down
                } else {
                    xkb::KeyDirection::Up
                },
            );
        }
        self.send_modifiers();
        Ok(())
    }

    fn send_modifiers(&self) {
        let (Some(keyboard), Some(state)) = (&self.keyboard, &self.xkb_state) else {
            return;
        };
        keyboard.modifiers(
            state.serialize_mods(xkb::STATE_MODS_DEPRESSED),
            state.serialize_mods(xkb::STATE_MODS_LATCHED),
            state.serialize_mods(xkb::STATE_MODS_LOCKED),
            state.serialize_layout(xkb::STATE_LAYOUT_EFFECTIVE),
        );
    }

    pub(crate) fn release_all(&mut self) -> Result<()> {
        let time = monotonic_millis()?;
        if let Some(pointer) = &self.pointer {
            for button in TRACKED_BUTTONS {
                let pressed = &mut self.pressed_buttons[button_index(button)];
                if *pressed {
                    *pressed = false;
                    pointer.button(time, button.evdev_code(), wl_pointer::ButtonState::Released);
                }
            }
            pointer.frame();
        }
        if let Some(keyboard) = &self.keyboard {
            for (key, pressed) in self.pressed_keys.iter_mut().enumerate() {
                if *pressed {
                    *pressed = false;
                    let key = u32::try_from(key).context("pressed key index exceeds u32")?;
                    keyboard.key(time, key, wl_keyboard::KeyState::Released as u32);
                    if let Some(state) = &mut self.xkb_state {
                        state.update_key(xkb::Keycode::new(key + 8), xkb::KeyDirection::Up);
                    }
                }
            }
            self.send_modifiers();
        }
        self.send_text(TextAction::Preedit, "")?;
        Ok(())
    }

    fn send_text(&self, action: TextAction, text: &str) -> Result<()> {
        if !self.method_active {
            return Ok(());
        }
        let Some(method) = &self.method else {
            return Ok(());
        };
        if action == TextAction::Preedit {
            let cursor = i32::try_from(text.len()).context("preedit text exceeds protocol size")?;
            method.set_preedit_string(text.to_owned(), cursor, cursor);
        } else {
            method.set_preedit_string(String::new(), 0, 0);
            method.commit_string(text.to_owned());
        }
        method.commit(self.method_serial);
        Ok(())
    }

    pub(crate) fn method_event(&mut self, event: &zwp_input_method_v2::Event) {
        match event {
            zwp_input_method_v2::Event::Activate => self.pending_method_active = Some(true),
            zwp_input_method_v2::Event::Deactivate => self.pending_method_active = Some(false),
            zwp_input_method_v2::Event::Done => {
                if let Some(active) = self.pending_method_active.take() {
                    self.method_active = active;
                }
                self.method_serial = self.method_serial.wrapping_add(1);
            }
            zwp_input_method_v2::Event::Unavailable => {
                self.method_active = false;
                if let Some(method) = self.method.take() {
                    method.destroy();
                }
            }
            zwp_input_method_v2::Event::SurroundingText { .. }
            | zwp_input_method_v2::Event::TextChangeCause { .. }
            | zwp_input_method_v2::Event::ContentType { .. }
            | _ => {}
        }
    }
}

fn monotonic_millis() -> Result<u32> {
    let timestamp = nix::time::clock_gettime(nix::time::ClockId::CLOCK_MONOTONIC)?;
    let millis = u64::try_from(timestamp.tv_sec())? * 1_000
        + u64::try_from(timestamp.tv_nsec())? / 1_000_000;
    Ok(u32::try_from(millis & u64::from(u32::MAX))?)
}

const TRACKED_BUTTONS: [PointerButton; 5] = [
    PointerButton::Left,
    PointerButton::Right,
    PointerButton::Middle,
    PointerButton::Side,
    PointerButton::Extra,
];

const fn button_index(button: PointerButton) -> usize {
    match button {
        PointerButton::Left => 0,
        PointerButton::Right => 1,
        PointerButton::Middle => 2,
        PointerButton::Side => 3,
        PointerButton::Extra => 4,
    }
}

pub(crate) fn valid_layout(layout: &str) -> bool {
    !layout.is_empty()
        && layout.len() <= 32
        && layout
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}
