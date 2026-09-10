use super::*;

#[test]
fn pointer_invariants_reject_values_the_decoder_cannot_accept() {
    assert!(PointerCoordinate::new(65_535).is_ok());
    assert!(PointerCoordinate::new(65_536).is_err());
    assert!(PointerDelta::new(-4096.0).is_ok());
    assert!(PointerDelta::new(4096.0).is_ok());
    assert!(PointerDelta::new(-4096.5).is_err());
    assert!(PointerDelta::new(4096.5).is_err());
    assert!(PointerDelta::new(f32::NAN).is_err());
    assert!(PointerDelta::new(f32::INFINITY).is_err());
}

#[test]
fn command_header_names_text_payload_presence_even_when_empty() {
    let empty_clipboard = [2, 7, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    let release_all = [2, 5, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];

    assert_eq!(
        CommandHeader::parse(&empty_clipboard)
            .expect("empty clipboard header should parse")
            .text_payload_len(),
        Some(0)
    );
    assert_eq!(
        CommandHeader::parse(&release_all)
            .expect("release-all header should parse")
            .text_payload_len(),
        None
    );
}

#[test]
fn cursor_encoding_reports_an_invalid_event() {
    let event = Event::CursorImage {
        size: CursorSize::new(1, 1).expect("one-pixel cursor size should be valid"),
        hotspot_x: 0,
        hotspot_y: 0,
        bgra: Vec::new(),
    };

    assert_eq!(event.encode(), Err(ProtocolError::InvalidEvent(4)));
}

fn command_case(kind: u8) -> Command {
    let sequence = InputSequence::new(7).expect("test input sequence should be valid");
    match kind {
        1 => Command::PointerAbsolute {
            x: PointerCoordinate::new(12).expect("test pointer x should be valid"),
            y: PointerCoordinate::new(34).expect("test pointer y should be valid"),
            sequence,
        },
        2 => Command::PointerButton {
            button: PointerButton::new(0x110).expect("test pointer button should be valid"),
            pressed: true,
            sequence,
        },
        3 => Command::PointerScroll {
            dx: PointerDelta::new(1.5).expect("test horizontal scroll should be valid"),
            dy: PointerDelta::new(-2.25).expect("test vertical scroll should be valid"),
            sequence,
        },
        4 => Command::KeyboardKey {
            key: KeyCode::new(30).expect("test key code should be valid"),
            state: KeyState::Repeated,
            sequence,
        },
        5 => Command::ReleaseAll,
        6 => Command::Resize {
            size: FrameSize::new(1280, 720).expect("test frame size should be valid"),
            scale_v120: ScaleV120::new(180).expect("test output scale should be valid"),
            request_id: RequestId::new(9).expect("test request ID should be valid"),
        },
        7 => Command::Clipboard(
            ClipboardText::new("clip".into()).expect("test clipboard text should be valid"),
        ),
        8 => Command::PointerRelative {
            dx: PointerDelta::new(1.5).expect("test relative x should be valid"),
            dy: PointerDelta::new(-2.25).expect("test relative y should be valid"),
            sequence,
        },
        9 => Command::Quality {
            bitrate_kbps: Kbps::new(8_000).expect("test bitrate should be valid"),
            fps: Fps::new(60).expect("test frame rate should be valid"),
            scale_percent: ScalePercent::new(75).expect("test scale should be valid"),
        },
        10 => Command::Text {
            action: TextAction::Preedit,
            text: InputText::new("hey".into()).expect("test input text should be valid"),
            sequence,
        },
        11 => Command::KeyframeReadiness {
            generation: Generation::new(4).expect("test generation should be valid"),
            ready: true,
        },
        _ => panic!("unknown test command"),
    }
}

fn command_bytes(kind: u8) -> Vec<u8> {
    match kind {
        1 => vec![2, 1, 0, 0, 12, 0, 0, 0, 34, 0, 0, 0, 7, 0, 0, 0],
        2 => vec![2, 2, 1, 0, 16, 1, 0, 0, 0, 0, 0, 0, 7, 0, 0, 0],
        3 => vec![2, 3, 0, 0, 0, 0, 192, 63, 0, 0, 16, 192, 7, 0, 0, 0],
        4 => vec![2, 4, 2, 0, 30, 0, 0, 0, 0, 0, 0, 0, 7, 0, 0, 0],
        5 => vec![2, 5, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
        6 => vec![2, 6, 0, 0, 0, 5, 0, 0, 208, 2, 0, 0, 180, 0, 9, 0],
        7 => vec![
            2, 7, 0, 0, 4, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 99, 108, 105, 112,
        ],
        8 => vec![2, 8, 0, 0, 0, 0, 192, 63, 0, 0, 16, 192, 7, 0, 0, 0],
        9 => vec![2, 9, 0, 0, 64, 31, 0, 0, 60, 0, 0, 0, 75, 0, 0, 0],
        10 => vec![
            2, 10, 1, 0, 3, 0, 0, 0, 7, 0, 0, 0, 0, 0, 0, 0, 104, 101, 121,
        ],
        11 => vec![2, 11, 1, 0, 4, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
        _ => panic!("unknown test command"),
    }
}

fn decode_command(bytes: &[u8]) -> Result<Command, ProtocolError> {
    let header = CommandHeader::parse(&bytes[..COMMAND_HEADER_BYTES])?;
    Command::decode(header, &bytes[COMMAND_HEADER_BYTES..])
}

macro_rules! command_wire_tests {
    ($encode_name:ident, $decode_name:ident, $kind:literal) => {
        #[test]
        fn $encode_name() {
            assert_eq!(command_case($kind).encode(), command_bytes($kind));
        }
        #[test]
        fn $decode_name() {
            assert_eq!(
                decode_command(&command_bytes($kind)),
                Ok(command_case($kind))
            );
        }
    };
}

command_wire_tests!(
    encode_pointer_absolute_exact,
    decode_pointer_absolute_exact,
    1
);
command_wire_tests!(encode_pointer_button_exact, decode_pointer_button_exact, 2);
command_wire_tests!(encode_pointer_scroll_exact, decode_pointer_scroll_exact, 3);
command_wire_tests!(encode_keyboard_key_exact, decode_keyboard_key_exact, 4);
command_wire_tests!(encode_release_all_exact, decode_release_all_exact, 5);
command_wire_tests!(encode_resize_exact, decode_resize_exact, 6);
command_wire_tests!(encode_clipboard_exact, decode_clipboard_exact, 7);
command_wire_tests!(
    encode_pointer_relative_exact,
    decode_pointer_relative_exact,
    8
);
command_wire_tests!(encode_quality_exact, decode_quality_exact, 9);
command_wire_tests!(encode_text_exact, decode_text_exact, 10);
command_wire_tests!(
    encode_keyframe_readiness_exact,
    decode_keyframe_readiness_exact,
    11
);

fn event_case(kind: u8) -> Event {
    match kind {
        1 => Event::Clipboard(
            ClipboardText::new("clip".into()).expect("test clipboard text should be valid"),
        ),
        2 => Event::Frame(FrameMetadata {
            generation: Generation::new(1).expect("test generation should be valid"),
            width: FrameDimension::new(1280).expect("test frame width should be valid"),
            height: FrameDimension::new(720).expect("test frame height should be valid"),
            capture_nanos: 2,
            sequence: 3,
            input_sequence: Some(
                InputSequence::new(4).expect("test input sequence should be valid"),
            ),
            fps: Fps::new(60).expect("test frame rate should be valid"),
        }),
        3 => Event::ResizeApplied {
            request_id: RequestId::new(9).expect("test request ID should be valid"),
            size: FrameSize::new(1280, 720).expect("test frame size should be valid"),
            scale_v120: ScaleV120::new(180).expect("test output scale should be valid"),
            generation: Generation::new(4).expect("test generation should be valid"),
        },
        4 => Event::CursorImage {
            size: CursorSize::new(1, 1).expect("test cursor size should be valid"),
            hotspot_x: -1,
            hotspot_y: 2,
            bgra: vec![1, 2, 3, 4],
        },
        5 => Event::CursorVisibility(true),
        _ => panic!("unknown test event"),
    }
}

fn event_bytes(kind: u8) -> Vec<u8> {
    match kind {
        1 => vec![2, 1, 0, 0, 4, 0, 0, 0, 99, 108, 105, 112],
        2 => vec![
            2, 2, 0, 0, 32, 0, 0, 0, 1, 0, 0, 0, 0, 5, 208, 2, 2, 0, 0, 0, 0, 0, 0, 0, 3, 0, 0, 0,
            0, 0, 0, 0, 4, 0, 0, 0, 60, 0, 0, 0,
        ],
        3 => vec![
            2, 3, 0, 0, 20, 0, 0, 0, 9, 0, 0, 0, 0, 5, 0, 0, 208, 2, 0, 0, 180, 0, 0, 0, 4, 0, 0, 0,
        ],
        4 => vec![
            2, 4, 0, 0, 20, 0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0, 255, 255, 255, 255, 2, 0, 0, 0, 1, 2,
            3, 4,
        ],
        5 => vec![2, 5, 0, 0, 1, 0, 0, 0, 1],
        _ => panic!("unknown test event"),
    }
}

fn decode_event(bytes: &[u8]) -> Result<Event, ProtocolError> {
    let header = EventHeader::parse(&bytes[..EVENT_HEADER_BYTES])?;
    Event::decode(header, &bytes[EVENT_HEADER_BYTES..])
}

macro_rules! event_wire_tests {
    ($encode_name:ident, $decode_name:ident, $kind:literal) => {
        #[test]
        fn $encode_name() {
            assert_eq!(event_case($kind).encode(), Ok(event_bytes($kind)));
        }
        #[test]
        fn $decode_name() {
            assert_eq!(decode_event(&event_bytes($kind)), Ok(event_case($kind)));
        }
    };
}

event_wire_tests!(
    encode_clipboard_event_exact,
    decode_clipboard_event_exact,
    1
);
event_wire_tests!(encode_frame_event_exact, decode_frame_event_exact, 2);
event_wire_tests!(encode_resize_event_exact, decode_resize_event_exact, 3);
event_wire_tests!(
    encode_cursor_image_event_exact,
    decode_cursor_image_event_exact,
    4
);
event_wire_tests!(
    encode_cursor_visibility_event_exact,
    decode_cursor_visibility_event_exact,
    5
);
