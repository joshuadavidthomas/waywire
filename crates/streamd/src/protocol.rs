use std::io::Read;
use std::io::{
    self,
};

use thiserror::Error;

pub const VERSION: u8 = 2;
pub const COMMAND_HEADER_BYTES: usize = 16;
pub const MAX_CLIPBOARD_BYTES: usize = 1024 * 1024;
pub const MAX_TEXT_BYTES: usize = 4_000;
pub const MAX_CURSOR_DIMENSION: u32 = 256;
pub const MAX_RAW_PIXELS: u64 = 3840 * 2160;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyState {
    Released,
    Pressed,
    Repeated,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    PointerAbsolute {
        x: u32,
        y: u32,
        sequence: u32,
    },
    PointerButton {
        button: u32,
        pressed: bool,
        sequence: u32,
    },
    PointerScroll {
        dx: f32,
        dy: f32,
        sequence: u32,
    },
    KeyboardKey {
        key: u32,
        state: KeyState,
        sequence: u32,
    },
    ReleaseAll,
    Resize {
        width: u32,
        height: u32,
        scale_v120: u16,
        request_id: u16,
    },
    Clipboard(String),
    PointerRelative {
        dx: f32,
        dy: f32,
        sequence: u32,
    },
    Quality {
        bitrate_kbps: u32,
        fps: u32,
        scale_percent: u32,
    },
    Text {
        preedit: bool,
        text: String,
        sequence: u32,
    },
    KeyframeReadiness {
        generation: u32,
        ready: bool,
    },
}

impl Command {
    pub fn input_sequence(&self) -> Option<u32> {
        match self {
            Self::PointerAbsolute { sequence, .. }
            | Self::PointerButton { sequence, .. }
            | Self::PointerScroll { sequence, .. }
            | Self::KeyboardKey { sequence, .. }
            | Self::PointerRelative { sequence, .. }
            | Self::Text { sequence, .. } => Some(*sequence),
            Self::ReleaseAll
            | Self::Resize { .. }
            | Self::Clipboard(_)
            | Self::Quality { .. }
            | Self::KeyframeReadiness { .. } => None,
        }
    }
}

#[derive(Debug, Error)]
pub enum ProtocolError {
    #[error("command stream ended halfway through a record")]
    Truncated,
    #[error("command header is invalid")]
    InvalidHeader,
    #[error("command fields are invalid for type {0}")]
    InvalidFields(u8),
    #[error("command payload exceeds its limit")]
    PayloadTooLarge,
    #[error("command payload is not valid text")]
    InvalidText,
    #[error("could not read command stream")]
    Io(#[source] io::Error),
}

pub struct CommandReader<R> {
    input: R,
}

impl<R: Read> CommandReader<R> {
    pub fn new(input: R) -> Self {
        Self { input }
    }

    pub fn next(&mut self) -> Result<Option<Command>, ProtocolError> {
        let mut header = [0_u8; COMMAND_HEADER_BYTES];
        let mut read = 0;
        while read < header.len() {
            match self.input.read(&mut header[read..]) {
                Ok(0) if read == 0 => return Ok(None),
                Ok(0) => return Err(ProtocolError::Truncated),
                Ok(count) => read += count,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(ProtocolError::Io(error)),
            }
        }
        if header[0] != VERSION || header[3] != 0 {
            return Err(ProtocolError::InvalidHeader);
        }
        let kind = header[1];
        let state = header[2];
        let a = u32::from_le_bytes([header[4], header[5], header[6], header[7]]);
        let b = u32::from_le_bytes([header[8], header[9], header[10], header[11]]);
        let c = u32::from_le_bytes([header[12], header[13], header[14], header[15]]);
        let invalid = || ProtocolError::InvalidFields(kind);
        let sequence = || if c == 0 { Err(invalid()) } else { Ok(c) };

        let command = match kind {
            1 if state == 0 && a <= 65_535 && b <= 65_535 => Command::PointerAbsolute {
                x: a,
                y: b,
                sequence: sequence()?,
            },
            2 if state <= 1 && is_button(a) && b == 0 => Command::PointerButton {
                button: a,
                pressed: state == 1,
                sequence: sequence()?,
            },
            3 if state == 0 => Command::PointerScroll {
                dx: bounded_float(a).ok_or_else(invalid)?,
                dy: bounded_float(b).ok_or_else(invalid)?,
                sequence: sequence()?,
            },
            4 if state <= 2 && (1..256).contains(&a) && b == 0 => Command::KeyboardKey {
                key: a,
                state: match state {
                    0 => KeyState::Released,
                    1 => KeyState::Pressed,
                    _ => KeyState::Repeated,
                },
                sequence: sequence()?,
            },
            5 if state == 0 && a == 0 && b == 0 && c == 0 => Command::ReleaseAll,
            6 if state == 0 => {
                let scale_v120 = (c & 0xffff) as u16;
                let request_id = (c >> 16) as u16;
                let pixels = u64::from(a) * u64::from(b);
                if !(320..=6_000).contains(&a)
                    || !(180..=6_000).contains(&b)
                    || a % 2 != 0
                    || b % 2 != 0
                    || pixels > MAX_RAW_PIXELS
                    || !(120..=480).contains(&scale_v120)
                    || request_id == 0
                {
                    return Err(invalid());
                }
                Command::Resize {
                    width: a,
                    height: b,
                    scale_v120,
                    request_id,
                }
            }
            7 if state == 0 && b == 0 && c == 0 => {
                Command::Clipboard(self.read_text(a, MAX_CLIPBOARD_BYTES, false)?)
            }
            8 if state == 0 => Command::PointerRelative {
                dx: bounded_float(a).ok_or_else(invalid)?,
                dy: bounded_float(b).ok_or_else(invalid)?,
                sequence: sequence()?,
            },
            9 if state == 0
                && (300..=50_000).contains(&a)
                && (10..=120).contains(&b)
                && (50..=100).contains(&c) =>
            {
                Command::Quality {
                    bitrate_kbps: a,
                    fps: b,
                    scale_percent: c,
                }
            }
            10 if state <= 1 && c == 0 => Command::Text {
                preedit: state == 1,
                text: self.read_text(a, MAX_TEXT_BYTES, true)?,
                sequence: if b == 0 { return Err(invalid()) } else { b },
            },
            11 if state <= 1 && a != 0 && b == 0 && c == 0 => Command::KeyframeReadiness {
                generation: a,
                ready: state == 1,
            },
            _ => return Err(invalid()),
        };
        Ok(Some(command))
    }

    fn read_text(
        &mut self,
        length: u32,
        limit: usize,
        forbid_nul: bool,
    ) -> Result<String, ProtocolError> {
        let length = usize::try_from(length).map_err(|_overflow| ProtocolError::PayloadTooLarge)?;
        if length > limit {
            return Err(ProtocolError::PayloadTooLarge);
        }
        let mut bytes = vec![0; length];
        let mut read = 0;
        while read < length {
            match self.input.read(&mut bytes[read..]) {
                Ok(0) => return Err(ProtocolError::Truncated),
                Ok(count) => read += count,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(ProtocolError::Io(error)),
            }
        }
        if forbid_nul && bytes.contains(&0) {
            return Err(ProtocolError::InvalidText);
        }
        String::from_utf8(bytes).map_err(|_invalid| ProtocolError::InvalidText)
    }
}

fn bounded_float(bits: u32) -> Option<f32> {
    let value = f32::from_bits(bits);
    (value.is_finite() && value.abs() <= 4096.0).then_some(value)
}

fn is_button(button: u32) -> bool {
    (0x110..=0x114).contains(&button)
}

pub struct CommandDecoder {
    bytes: Vec<u8>,
}

impl CommandDecoder {
    pub fn new() -> Self {
        Self { bytes: Vec::new() }
    }

    pub fn push(&mut self, incoming: &[u8]) -> Result<Vec<Command>, ProtocolError> {
        if self.bytes.len().saturating_add(incoming.len())
            > MAX_CLIPBOARD_BYTES + COMMAND_HEADER_BYTES + 64 * 1024
        {
            return Err(ProtocolError::PayloadTooLarge);
        }
        self.bytes.extend_from_slice(incoming);
        let mut commands = Vec::new();
        loop {
            if self.bytes.len() < COMMAND_HEADER_BYTES {
                break;
            }
            let kind = self.bytes[1];
            let payload = if matches!(kind, 7 | 10) {
                let length = u32::from_le_bytes([
                    self.bytes[4],
                    self.bytes[5],
                    self.bytes[6],
                    self.bytes[7],
                ]) as usize;
                let limit = if kind == 7 {
                    MAX_CLIPBOARD_BYTES
                } else {
                    MAX_TEXT_BYTES
                };
                if length > limit {
                    return Err(ProtocolError::PayloadTooLarge);
                }
                length
            } else {
                0
            };
            let record_length = COMMAND_HEADER_BYTES + payload;
            if self.bytes.len() < record_length {
                break;
            }
            let record: Vec<u8> = self.bytes.drain(..record_length).collect();
            let mut reader = CommandReader::new(io::Cursor::new(record));
            commands.push(reader.next()?.ok_or(ProtocolError::Truncated)?);
        }
        Ok(commands)
    }

    pub fn finish(self) -> Result<(), ProtocolError> {
        if self.bytes.is_empty() {
            Ok(())
        } else {
            Err(ProtocolError::Truncated)
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameMetadata {
    pub generation: u32,
    pub width: u16,
    pub height: u16,
    pub capture_nanos: u64,
    pub sequence: u64,
    pub input_sequence: u32,
    pub fps: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Clipboard(String),
    Frame(FrameMetadata),
    ResizeApplied {
        request_id: u16,
        width: u32,
        height: u32,
        scale_v120: u16,
        generation: u32,
    },
    CursorImage {
        width: u32,
        height: u32,
        hotspot_x: i32,
        hotspot_y: i32,
        bgra: Vec<u8>,
    },
    CursorVisibility(bool),
}

impl Event {
    pub fn encode(&self) -> Result<Vec<u8>, ProtocolError> {
        let (kind, payload) = match self {
            Self::Clipboard(text) => {
                if text.len() > MAX_CLIPBOARD_BYTES {
                    return Err(ProtocolError::PayloadTooLarge);
                }
                (1, text.as_bytes().to_vec())
            }
            Self::Frame(frame) => {
                if frame.generation == 0 || frame.width == 0 || frame.height == 0 || frame.fps == 0
                {
                    return Err(ProtocolError::InvalidFields(2));
                }
                let mut payload = vec![0; 32];
                payload[0..4].copy_from_slice(&frame.generation.to_le_bytes());
                payload[4..6].copy_from_slice(&frame.width.to_le_bytes());
                payload[6..8].copy_from_slice(&frame.height.to_le_bytes());
                payload[8..16].copy_from_slice(&frame.capture_nanos.to_le_bytes());
                payload[16..24].copy_from_slice(&frame.sequence.to_le_bytes());
                payload[24..28].copy_from_slice(&frame.input_sequence.to_le_bytes());
                payload[28..32].copy_from_slice(&frame.fps.to_le_bytes());
                (2, payload)
            }
            Self::ResizeApplied {
                request_id,
                width,
                height,
                scale_v120,
                generation,
            } => {
                if *request_id == 0 || *generation == 0 {
                    return Err(ProtocolError::InvalidFields(3));
                }
                let mut payload = vec![0; 20];
                payload[0..2].copy_from_slice(&request_id.to_le_bytes());
                payload[4..8].copy_from_slice(&width.to_le_bytes());
                payload[8..12].copy_from_slice(&height.to_le_bytes());
                payload[12..14].copy_from_slice(&scale_v120.to_le_bytes());
                payload[16..20].copy_from_slice(&generation.to_le_bytes());
                (3, payload)
            }
            Self::CursorImage {
                width,
                height,
                hotspot_x,
                hotspot_y,
                bgra,
            } => {
                let expected = cursor_byte_count(*width, *height)?;
                if bgra.len() != expected {
                    return Err(ProtocolError::InvalidFields(4));
                }
                let mut payload = Vec::with_capacity(16 + expected);
                payload.extend_from_slice(&width.to_le_bytes());
                payload.extend_from_slice(&height.to_le_bytes());
                payload.extend_from_slice(&hotspot_x.to_le_bytes());
                payload.extend_from_slice(&hotspot_y.to_le_bytes());
                payload.extend_from_slice(bgra);
                (4, payload)
            }
            Self::CursorVisibility(visible) => (5, vec![u8::from(*visible)]),
        };
        let length =
            u32::try_from(payload.len()).map_err(|_overflow| ProtocolError::PayloadTooLarge)?;
        let mut bytes = Vec::with_capacity(8 + payload.len());
        bytes.extend_from_slice(&[VERSION, kind, 0, 0]);
        bytes.extend_from_slice(&length.to_le_bytes());
        bytes.extend_from_slice(&payload);
        Ok(bytes)
    }
}

pub fn cursor_byte_count(width: u32, height: u32) -> Result<usize, ProtocolError> {
    if width == 0 || height == 0 || width > MAX_CURSOR_DIMENSION || height > MAX_CURSOR_DIMENSION {
        return Err(ProtocolError::InvalidFields(4));
    }
    usize::try_from(u64::from(width) * u64::from(height) * 4)
        .map_err(|_overflow| ProtocolError::PayloadTooLarge)
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    fn header(kind: u8, state: u8, a: u32, b: u32, c: u32) -> Vec<u8> {
        let mut bytes = vec![VERSION, kind, state, 0];
        bytes.extend(a.to_le_bytes());
        bytes.extend(b.to_le_bytes());
        bytes.extend(c.to_le_bytes());
        bytes
    }

    #[test]
    fn parses_fragmented_text_and_exact_golden_bytes() {
        struct OneByte<R>(R);
        impl<R: Read> Read for OneByte<R> {
            fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
                let length = output.len().min(1);
                self.0.read(&mut output[..length])
            }
        }
        let mut bytes = header(10, 1, 3, 42, 0);
        bytes.extend_from_slice(b"hey");
        let mut reader = CommandReader::new(OneByte(Cursor::new(bytes)));
        assert_eq!(
            reader.next().expect("fragmented text should parse"),
            Some(Command::Text {
                preedit: true,
                text: "hey".into(),
                sequence: 42
            })
        );
        assert_eq!(reader.next().expect("stream should end cleanly"), None);
    }

    #[test]
    fn rejects_truncated_oversized_and_nonfinite_records() {
        assert!(matches!(
            CommandReader::new(Cursor::new(vec![2])).next(),
            Err(ProtocolError::Truncated)
        ));
        assert!(matches!(
            CommandReader::new(Cursor::new(header(
                7,
                0,
                u32::try_from(MAX_CLIPBOARD_BYTES)
                    .expect("clipboard limit should fit the protocol length field")
                    + 1,
                0,
                0
            )))
            .next(),
            Err(ProtocolError::PayloadTooLarge)
        ));
        assert!(
            CommandReader::new(Cursor::new(header(8, 0, f32::NAN.to_bits(), 0, 1)))
                .next()
                .is_err()
        );
    }

    #[test]
    fn resize_and_private_readiness_have_distinct_shapes() {
        let packed = u32::from(180_u16) | (u32::from(7_u16) << 16);
        assert_eq!(
            CommandReader::new(Cursor::new(header(6, 0, 3840, 2160, packed)))
                .next()
                .expect("resize command should parse"),
            Some(Command::Resize {
                width: 3840,
                height: 2160,
                scale_v120: 180,
                request_id: 7
            })
        );
        assert_eq!(
            CommandReader::new(Cursor::new(header(11, 1, 9, 0, 0)))
                .next()
                .expect("readiness command should parse"),
            Some(Command::KeyframeReadiness {
                generation: 9,
                ready: true
            })
        );
    }

    #[test]
    fn resize_parser_enforces_the_native_four_k_pixel_budget() {
        let packed = u32::from(120_u16) | (u32::from(1_u16) << 16);
        assert!(
            CommandReader::new(Cursor::new(header(6, 0, 3840, 2160, packed)))
                .next()
                .is_ok()
        );
        assert!(matches!(
            CommandReader::new(Cursor::new(header(6, 0, 3842, 2160, packed))).next(),
            Err(ProtocolError::InvalidFields(6))
        ));
        assert!(matches!(
            CommandReader::new(Cursor::new(header(6, 0, 6000, 6000, packed))).next(),
            Err(ProtocolError::InvalidFields(6))
        ));
    }

    #[test]
    fn frame_event_matches_reference_golden_bytes() {
        let event = Event::Frame(FrameMetadata {
            generation: 1,
            width: 1280,
            height: 720,
            capture_nanos: 2,
            sequence: 3,
            input_sequence: 4,
            fps: 60,
        });
        let bytes = event.encode().expect("frame event should encode");
        assert_eq!(&bytes[..8], &[2, 2, 0, 0, 32, 0, 0, 0]);
        assert_eq!(&bytes[8..12], &1_u32.to_le_bytes());
        assert_eq!(&bytes[12..14], &1280_u16.to_le_bytes());
        assert_eq!(&bytes[14..16], &720_u16.to_le_bytes());
        assert_eq!(&bytes[16..24], &2_u64.to_le_bytes());
        assert_eq!(&bytes[24..32], &3_u64.to_le_bytes());
        assert_eq!(&bytes[32..36], &4_u32.to_le_bytes());
        assert_eq!(&bytes[36..40], &60_u32.to_le_bytes());
    }

    #[test]
    fn cursor_payload_is_bounded_and_exact() {
        assert_eq!(
            cursor_byte_count(256, 256).expect("max cursor should be sized"),
            256 * 256 * 4
        );
        assert!(cursor_byte_count(257, 1).is_err());
        let bytes = Event::CursorImage {
            width: 1,
            height: 1,
            hotspot_x: -1,
            hotspot_y: 2,
            bgra: vec![1, 2, 3, 4],
        }
        .encode()
        .expect("cursor image should encode");
        assert_eq!(&bytes[..8], &[2, 4, 0, 0, 20, 0, 0, 0]);
        assert_eq!(&bytes[16..20], &(-1_i32).to_le_bytes());
    }
}
