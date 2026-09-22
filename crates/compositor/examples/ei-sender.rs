//! Direct reis sender used by tests/interop.py. Commands use evdev codes and
//! compositor-logical coordinates, not browser protocol coordinates.
use std::io::BufRead;
use std::io::Write;
use std::io::{
    self,
};
use std::os::unix::net::UnixStream;

use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use smithay::reexports::reis::ei;
use smithay::reexports::reis::event::DeviceCapability;
use smithay::reexports::reis::event::EiEvent;

#[expect(
    clippy::too_many_lines,
    reason = "single linear test-client command loop"
)]
#[expect(
    clippy::wildcard_enum_match_arm,
    reason = "sender only handles device lifecycle events"
)]
fn main() -> Result<()> {
    let path = std::env::args().nth(1).context("EI socket path required")?;
    let context = ei::Context::new(UnixStream::connect(path)?)?;
    let (connection, mut events) =
        context.handshake_blocking("waywire-test", ei::handshake::ContextType::Sender)?;
    let mut keyboard = None;
    let mut pointer = None;
    for event in events.by_ref() {
        match event? {
            EiEvent::SeatAdded(event) => {
                event.seat.bind_capabilities(
                    DeviceCapability::Keyboard
                        | DeviceCapability::PointerAbsolute
                        | DeviceCapability::Button,
                );
                context.flush()?;
            }
            EiEvent::DeviceResumed(event) => {
                event.device.device().start_emulating(1, event.serial);
                if event.device.interface::<ei::Keyboard>().is_some() {
                    keyboard = Some(event.device.clone());
                }
                if event.device.interface::<ei::PointerAbsolute>().is_some() {
                    pointer = Some(event.device);
                }
                context.flush()?;
                if keyboard.is_some() && pointer.is_some() {
                    break;
                }
            }
            _ => {}
        }
    }
    let keyboard = keyboard.context("server did not create keyboard")?;
    let mut pointer = pointer.context("server did not create absolute pointer")?;
    println!("ready");
    io::stdout().flush()?;
    for (frame, line) in io::stdin().lock().lines().enumerate() {
        let line = line?;
        let words: Vec<_> = line.split_whitespace().collect();
        let device = match words.as_slice() {
            ["refresh"] => {
                for event in events.by_ref() {
                    if let EiEvent::DeviceResumed(event) = event?
                        && event.device.interface::<ei::PointerAbsolute>().is_some()
                    {
                        event.device.device().start_emulating(2, event.serial);
                        pointer = event.device;
                        break;
                    }
                }
                context.flush()?;
                println!("sent {line}");
                io::stdout().flush()?;
                continue;
            }
            ["region"] => {
                let region = pointer.regions().first().context("no advertised region")?;
                println!("region {} {} {}", region.width, region.height, region.scale);
                io::stdout().flush()?;
                continue;
            }
            ["key", code, state] => {
                keyboard
                    .interface::<ei::Keyboard>()
                    .context("keyboard interface")?
                    .key(
                        code.parse()?,
                        if *state == "1" {
                            ei::keyboard::KeyState::Press
                        } else {
                            ei::keyboard::KeyState::Released
                        },
                    );
                &keyboard
            }
            ["button", code, state] => {
                pointer
                    .interface::<ei::Button>()
                    .context("button interface")?
                    .button(
                        code.parse()?,
                        if *state == "1" {
                            ei::button::ButtonState::Press
                        } else {
                            ei::button::ButtonState::Released
                        },
                    );
                &pointer
            }
            ["motion", x, y] => {
                pointer
                    .interface::<ei::PointerAbsolute>()
                    .context("absolute interface")?
                    .motion_absolute(x.parse()?, y.parse()?);
                &pointer
            }
            ["close-keyboard"] => {
                keyboard.device().release();
                context.flush()?;
                println!("sent {line}");
                io::stdout().flush()?;
                continue;
            }
            _ => bail!("invalid sender command {line}"),
        };
        device
            .device()
            .frame(connection.serial(), (frame as u64 + 1) * 1000);
        context.flush()?;
        println!("sent {line}");
        io::stdout().flush()?;
    }
    Ok(())
}
