use super::*;

fn value<T>(result: Result<T, InvalidValue>) -> T {
    result.expect("test protocol value should be valid")
}

fn command_cases() -> Vec<(&'static str, Command, Vec<u8>)> {
    let sequence = value(InputSequence::new(7));
    vec![
        (
            "pointer absolute",
            Command::PointerAbsolute {
                x: value(PointerCoordinate::new(12)),
                y: value(PointerCoordinate::new(34)),
                sequence,
            },
            vec![2, 1, 0, 0, 12, 0, 0, 0, 34, 0, 0, 0, 7, 0, 0, 0],
        ),
        (
            "pointer button",
            Command::PointerButton {
                button: PointerButton::Left,
                pressed: true,
                sequence,
            },
            vec![2, 2, 1, 0, 16, 1, 0, 0, 0, 0, 0, 0, 7, 0, 0, 0],
        ),
        (
            "pointer scroll",
            Command::PointerScroll {
                dx: value(PointerDelta::new(1.5)),
                dy: value(PointerDelta::new(-2.25)),
                sequence,
            },
            vec![2, 3, 0, 0, 0, 0, 192, 63, 0, 0, 16, 192, 7, 0, 0, 0],
        ),
        (
            "keyboard key",
            Command::KeyboardKey {
                key: value(KeyCode::new(30)),
                state: KeyState::Repeated,
                sequence,
            },
            vec![2, 4, 2, 0, 30, 0, 0, 0, 0, 0, 0, 0, 7, 0, 0, 0],
        ),
        (
            "release all",
            Command::ReleaseAll,
            vec![2, 5, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
        ),
        (
            "resize",
            Command::Resize {
                size: value(FrameSize::new(1280, 720)),
                scale_v120: value(ScaleV120::new(180)),
                request_id: value(RequestId::new(9)),
            },
            vec![2, 6, 0, 0, 0, 5, 0, 0, 208, 2, 0, 0, 180, 0, 9, 0],
        ),
        (
            "clipboard",
            Command::Clipboard(value(ClipboardText::new("clip".into()))),
            vec![
                2, 7, 0, 0, 4, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 99, 108, 105, 112,
            ],
        ),
        (
            "pointer relative",
            Command::PointerRelative {
                dx: value(PointerDelta::new(1.5)),
                dy: value(PointerDelta::new(-2.25)),
                sequence,
            },
            vec![2, 8, 0, 0, 0, 0, 192, 63, 0, 0, 16, 192, 7, 0, 0, 0],
        ),
        (
            "quality",
            Command::Quality {
                bitrate_kbps: value(Kbps::new(8_000)),
                fps: value(Fps::new(60)),
                scale_percent: value(ScalePercent::new(75)),
            },
            vec![2, 9, 0, 0, 64, 31, 0, 0, 60, 0, 0, 0, 75, 0, 0, 0],
        ),
        (
            "text",
            Command::Text {
                action: TextAction::Preedit,
                text: value(InputText::new("hey".into())),
                sequence,
            },
            vec![
                2, 10, 1, 0, 3, 0, 0, 0, 7, 0, 0, 0, 0, 0, 0, 0, 104, 101, 121,
            ],
        ),
        (
            "keyframe readiness",
            Command::KeyframeReadiness {
                generation: value(Generation::new(4)),
                ready: true,
            },
            vec![2, 11, 1, 0, 4, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
        ),
    ]
}

fn event_cases() -> Vec<(&'static str, Event, Vec<u8>)> {
    vec![
        (
            "clipboard",
            Event::Clipboard(value(ClipboardText::new("clip".into()))),
            vec![2, 1, 0, 0, 4, 0, 0, 0, 99, 108, 105, 112],
        ),
        (
            "frame",
            Event::Frame(FrameMetadata {
                generation: value(Generation::new(1)),
                width: value(FrameDimension::new(1280)),
                height: value(FrameDimension::new(720)),
                capture_nanos: 2,
                sequence: 3,
                input_sequence: Some(value(InputSequence::new(4))),
                fps: value(Fps::new(60)),
            }),
            vec![
                2, 2, 0, 0, 32, 0, 0, 0, 1, 0, 0, 0, 0, 5, 208, 2, 2, 0, 0, 0, 0, 0, 0, 0, 3, 0, 0,
                0, 0, 0, 0, 0, 4, 0, 0, 0, 60, 0, 0, 0,
            ],
        ),
        (
            "resize applied",
            Event::ResizeApplied {
                request_id: value(RequestId::new(9)),
                size: value(FrameSize::new(1280, 720)),
                scale_v120: value(ScaleV120::new(180)),
                generation: value(Generation::new(4)),
            },
            vec![
                2, 3, 0, 0, 20, 0, 0, 0, 9, 0, 0, 0, 0, 5, 0, 0, 208, 2, 0, 0, 180, 0, 0, 0, 4, 0,
                0, 0,
            ],
        ),
        (
            "cursor image",
            Event::CursorImage(value(CursorImage::new(
                value(CursorSize::new(1, 1)),
                -1,
                2,
                vec![1, 2, 3, 4],
            ))),
            vec![
                2, 4, 0, 0, 20, 0, 0, 0, 1, 0, 0, 0, 1, 0, 0, 0, 255, 255, 255, 255, 2, 0, 0, 0, 1,
                2, 3, 4,
            ],
        ),
        (
            "cursor visibility",
            Event::CursorVisibility(true),
            vec![2, 5, 0, 0, 1, 0, 0, 0, 1],
        ),
    ]
}

fn decode_command(bytes: &[u8]) -> Result<Command, ProtocolError> {
    let header = CommandHeader::parse(&bytes[..COMMAND_HEADER_BYTES])?;
    Command::decode(header, &bytes[COMMAND_HEADER_BYTES..])
}

fn decode_event(bytes: &[u8]) -> Result<Event, ProtocolError> {
    let header = EventHeader::parse(&bytes[..EVENT_HEADER_BYTES])?;
    Event::decode(header, &bytes[EVENT_HEADER_BYTES..])
}

#[test]
fn every_command_encodes_to_its_pinned_bytes() {
    for (name, command, bytes) in command_cases() {
        assert_eq!(command.encode(), bytes, "{name}");
    }
}

#[test]
fn every_pinned_command_record_decodes() {
    for (name, command, bytes) in command_cases() {
        assert_eq!(decode_command(&bytes), Ok(command), "{name}");
    }
}

#[test]
fn every_event_encodes_to_its_pinned_bytes() {
    for (name, event, bytes) in event_cases() {
        assert_eq!(event.encode(), bytes, "{name}");
    }
}

#[test]
fn every_pinned_event_record_decodes() {
    for (name, event, bytes) in event_cases() {
        assert_eq!(decode_event(&bytes), Ok(event), "{name}");
    }
}

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
fn wrong_version_is_an_invalid_header() {
    let mut header = [0; COMMAND_HEADER_BYTES];
    header[0] = VERSION - 1;
    header[1] = 5;
    assert_eq!(
        CommandHeader::parse(&header),
        Err(ProtocolError::InvalidHeader)
    );
}

#[test]
fn unknown_command_kind_is_invalid_fields() {
    let mut header = [0; COMMAND_HEADER_BYTES];
    header[0] = VERSION;
    header[1] = 99;
    assert_eq!(
        CommandHeader::parse(&header),
        Err(ProtocolError::InvalidFields(99))
    );
}

#[test]
fn unknown_event_kind_is_invalid_event() {
    let header = [VERSION, 99, 0, 0, 0, 0, 0, 0];
    assert_eq!(
        EventHeader::parse(&header),
        Err(ProtocolError::InvalidEvent(99))
    );
}

#[test]
fn command_text_payload_over_limit_is_too_large() {
    let mut header = [0; COMMAND_HEADER_BYTES];
    header[0] = VERSION;
    header[1] = 10;
    header[4..8].copy_from_slice(
        &(u32::try_from(MAX_TEXT_BYTES).expect("limit fits u32") + 1).to_le_bytes(),
    );
    assert_eq!(
        CommandHeader::parse(&header),
        Err(ProtocolError::PayloadTooLarge)
    );
}

#[test]
fn event_payload_over_limit_is_too_large() {
    let mut header = [VERSION, 1, 0, 0, 0, 0, 0, 0];
    header[4..8].copy_from_slice(
        &(u32::try_from(MAX_EVENT_BYTES).expect("limit fits u32") + 1).to_le_bytes(),
    );
    assert_eq!(
        EventHeader::parse(&header),
        Err(ProtocolError::PayloadTooLarge)
    );
}

#[test]
fn clipboard_payload_must_be_utf8() {
    let header = EventHeader::parse(&[VERSION, 1, 0, 0, 1, 0, 0, 0])
        .expect("clipboard event header should parse");
    assert_eq!(Event::decode(header, &[0xff]), Err(ProtocolError::NotUtf8));
}

#[test]
fn cursor_image_payload_length_must_match_its_size() {
    let header = EventHeader::parse(&[VERSION, 4, 0, 0, 16, 0, 0, 0])
        .expect("cursor image event header should parse");
    let mut payload = [0; 16];
    payload[0..4].copy_from_slice(&1_u32.to_le_bytes());
    payload[4..8].copy_from_slice(&1_u32.to_le_bytes());
    assert_eq!(
        Event::decode(header, &payload),
        Err(ProtocolError::InvalidEvent(4))
    );
}

#[test]
fn command_payload_length_must_match_its_header() {
    let header = CommandHeader::parse(&[VERSION, 7, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0])
        .expect("clipboard command header should parse");
    assert_eq!(Command::decode(header, &[]), Err(ProtocolError::Truncated));
}

#[test]
fn input_text_rejects_nul() {
    assert!(InputText::new("a\0b".into()).is_err());
}

#[test]
fn clipboard_text_rejects_oversize_input() {
    assert!(ClipboardText::new("x".repeat(MAX_CLIPBOARD_BYTES + 1)).is_err());
}

#[test]
fn cursor_image_rejects_wrong_length_buffer() {
    let size = value(CursorSize::new(1, 1));
    assert!(CursorImage::new(size, 0, 0, Vec::new()).is_err());
}
