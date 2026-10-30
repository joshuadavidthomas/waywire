use std::collections::HashSet;
use std::collections::VecDeque;
use std::fmt::Write;

use smithay::backend::input::InputTime;
use smithay::backend::input::KeyState;
use smithay::input::keyboard::FilterResult;
use smithay::input::keyboard::KeyboardSource;
use smithay::input::keyboard::KeymapFileId;
use smithay::utils::SERIAL_COUNTER;

use crate::State;

#[derive(Default)]
pub(crate) struct TextKeyboard {
    pub(crate) pending: VecDeque<String>,
    base: Option<String>,
    installed: Option<KeymapFileId>,
    slots: Vec<Slot>,
    clock: u64,
}

struct Slot {
    code: u32,
    name: String,
    new_name: bool,
    character: Option<char>,
    last_used: u64,
}

impl TextKeyboard {
    fn assign(&mut self, text: &str, wayland: bool) -> (Vec<u32>, bool) {
        let eligible = |slot: &Slot| (slot.code > 255) == wayland;
        let capacity = self.slots.iter().filter(|slot| eligible(slot)).count();
        let mut characters = HashSet::new();
        for character in text.chars() {
            if characters.len() < capacity || characters.contains(&character) {
                characters.insert(character);
            }
        }
        let mut used: HashSet<usize> = self
            .slots
            .iter()
            .enumerate()
            .filter(|(_, slot)| {
                eligible(slot) && slot.character.is_some_and(|c| characters.contains(&c))
            })
            .map(|(index, _)| index)
            .collect();
        let mut codes = Vec::new();
        let mut changed = false;
        for character in text.chars() {
            if !characters.contains(&character) {
                tracing::warn!(%character, "text keyboard spare keycodes exhausted; character dropped");
                continue;
            }
            let index = self
                .slots
                .iter()
                .position(|s| eligible(s) && s.character == Some(character))
                .or_else(|| {
                    self.slots
                        .iter()
                        .enumerate()
                        .filter(|(index, slot)| eligible(slot) && !used.contains(index))
                        .min_by_key(|(_, slot)| slot.last_used)
                        .map(|(index, _)| index)
                });
            let Some(index) = index else { continue };
            used.insert(index);
            self.clock += 1;
            let slot = &mut self.slots[index];
            if slot.character != Some(character) {
                slot.character = Some(character);
                changed = true;
            }
            slot.last_used = self.clock;
            codes.push(slot.code);
        }
        (codes, changed)
    }

    fn keymap(&self) -> Option<String> {
        let mut keymap = self.base.clone()?;
        let symbols_end = keymap.rfind("};\n\n};")?;
        let mut symbols = String::new();
        let mut names = String::new();
        for slot in &self.slots {
            if let Some(c) = slot.character {
                writeln!(
                    symbols,
                    "\tkey <{}> {{ [ U{:04X} ] }};",
                    slot.name,
                    u32::from(c)
                )
                .ok()?;
                if slot.new_name {
                    writeln!(names, "\t<{}> = {};", slot.name, slot.code).ok()?;
                }
            }
        }
        keymap.insert_str(symbols_end, &symbols);
        let keycodes_end = keymap.find("};\n\nxkb_types")?;
        keymap.insert_str(keycodes_end, &names);
        Some(keymap)
    }
}

impl State {
    fn init_text_keyboard(&mut self) {
        let Some(keyboard) = self.seat.get_keyboard() else {
            return;
        };
        let result = keyboard.with_xkb_state(self, |context| {
            let Ok(guard) = context.xkb().lock() else {
                return None;
            };
            // SAFETY: The keymap reference does not outlive the lock or acquire a ref-count.
            let keymap = unsafe { guard.keymap() };
            let mut slots = Vec::new();
            for code in 8..=511 {
                let key = code.into();
                let used = (0..keymap.num_layouts_for_key(key)).any(|layout| {
                    (0..keymap.num_levels_for_key(key, layout))
                        .any(|level| !keymap.key_get_syms_by_level(key, layout, level).is_empty())
                });
                if !used {
                    let name = keymap.key_get_name(key).map(str::to_owned);
                    slots.push(Slot {
                        code,
                        new_name: name.is_none(),
                        name: name.unwrap_or_else(|| format!("W{code:03}")),
                        character: None,
                        last_used: 0,
                    });
                }
            }
            Some((guard.keymap_as_string(), slots))
        });
        if let Some((base, slots)) = result {
            tracing::info!(
                x11 = slots.iter().filter(|slot| slot.code <= 255).count(),
                wayland = slots.iter().filter(|slot| slot.code > 255).count(),
                "text keyboard spare keycodes"
            );
            self.text_keyboard.base = Some(base);
            self.text_keyboard.slots = slots;
        }
    }

    pub(crate) fn flush_text_keyboard(&mut self) {
        let Some(keyboard) = self.seat.get_keyboard() else {
            return;
        };
        if !keyboard.pressed_keys().is_empty() || self.text_keyboard.pending.is_empty() {
            return;
        }
        if self
            .text_keyboard
            .installed
            .is_some_and(|id| keyboard.keymap_file().id() != id)
        {
            self.text_keyboard.base = None;
            self.text_keyboard.installed = None;
            self.text_keyboard.slots.clear();
        }
        if self.text_keyboard.base.is_none() {
            self.init_text_keyboard();
        }
        let wayland = matches!(
            keyboard.current_focus(),
            Some(crate::focus::KeyboardFocus::Wayland(_))
        );
        while let Some(text) = self.text_keyboard.pending.pop_front() {
            let (codes, changed) = self.text_keyboard.assign(&text, wayland);
            if changed {
                let Some(keymap) = self.text_keyboard.keymap() else {
                    tracing::warn!("unexpected XKB keymap format");
                    self.text_keyboard.base = None;
                    return;
                };
                let modifiers = keyboard.modifier_state();
                if let Err(error) = keyboard.set_keymap_from_string(self, keymap) {
                    tracing::warn!(%error, "text keyboard keymap failed");
                    self.text_keyboard.base = None;
                    return;
                }
                self.text_keyboard.installed = Some(keyboard.keymap_file().id());
                keyboard.set_modifier_state(modifiers);
                keyboard.advertise_modifier_state(self);
            }
            for code in codes {
                for state in [KeyState::Pressed, KeyState::Released] {
                    keyboard.input_from_source::<(), _>(
                        KeyboardSource::MAIN,
                        self,
                        code.into(),
                        state,
                        SERIAL_COUNTER.next_serial(),
                        InputTime::now(),
                        |_, _, _| FilterResult::Forward,
                    );
                }
            }
        }
    }
}
