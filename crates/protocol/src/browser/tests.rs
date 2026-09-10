use super::*;

fn value<T>(result: Result<T, pipe::ProtocolError>) -> T {
    result.expect("test protocol value should be valid")
}

fn json(event: &ClientEvent) -> String {
    serde_json::to_string(event).expect("test client event should serialize")
}

fn record(kind: u8, state: u8, a: u32, b: u32, c: u32) -> Vec<u8> {
    let mut bytes = vec![2, kind, state, 0];
    bytes.extend(a.to_le_bytes());
    bytes.extend(b.to_le_bytes());
    bytes.extend(c.to_le_bytes());
    bytes
}

#[test]
fn video_config_json_is_exact() {
    let event = ClientEvent::video_config("avc1.F40034".into(), value(Fps::new(60)));
    assert_eq!(
        json(&event),
        r#"{"codec":"avc1.F40034","frameRate":60,"type":"video-config","version":2}"#
    );
}

#[test]
fn cursor_json_is_exact() {
    let cursor = CursorState::new().with_image(1, 2, -3, 4, "data:image/png;base64,AA==".into());
    assert_eq!(
        json(&ClientEvent::Cursor(cursor)),
        r#"{"type":"cursor","visible":true,"width":1,"height":2,"hotspotX":-3,"hotspotY":4,"image":"data:image/png;base64,AA=="}"#
    );
}

#[test]
fn clipboard_json_is_exact() {
    assert_eq!(
        json(&ClientEvent::Clipboard {
            text: value(ClipboardText::new("hello".into()))
        }),
        r#"{"text":"hello","type":"clipboard"}"#
    );
}

#[test]
fn resize_applied_json_is_exact() {
    let event = ClientEvent::ResizeApplied {
        request: value(pipe::RequestId::new(9)),
        width: 1280,
        height: 720,
        scale: value(pipe::ScaleV120::new(180)),
        generation: value(pipe::Generation::new(4)),
    };
    assert_eq!(
        json(&event),
        r#"{"generation":4,"height":720,"request":9,"scale":180,"type":"resize-applied","width":1280}"#
    );
}

#[test]
fn quality_json_is_exact() {
    let event = ClientEvent::Quality(QualityLevels {
        bitrate: value(Kbps::new(8_000)),
        fps: value(Fps::new(60)),
        scale: value(ScalePercent::new(75)),
    });
    assert_eq!(
        json(&event),
        r#"{"bitrate":8000,"fps":60,"scale":75,"type":"quality"}"#
    );
}

#[test]
fn control_state_json_is_exact() {
    assert_eq!(
        json(&ClientEvent::ControlState {
            state: ControlState::Active
        }),
        r#"{"state":"active","type":"control-state"}"#
    );
}

#[test]
fn pong_json_is_exact() {
    assert_eq!(
        json(&ClientEvent::Pong {
            id: 3,
            server_nanos: "99".into()
        }),
        r#"{"id":3,"serverNanos":"99","type":"pong"}"#
    );
}

macro_rules! browser_record_tests {
    ($accepted_name:ident, $rejected_name:ident, $accepted:expr, $rejected:expr, $variant:pat) => {
        #[test]
        fn $accepted_name() {
            assert!(matches!(parse_browser_record(&$accepted), Ok($variant)));
        }

        #[test]
        fn $rejected_name() {
            assert!(parse_browser_record(&$rejected).is_err());
        }
    };
}

browser_record_tests!(
    accepts_pointer_absolute,
    rejects_pointer_absolute,
    record(1, 0, 12, 34, 7),
    record(1, 1, 12, 34, 7),
    Command::PointerAbsolute { .. }
);
browser_record_tests!(
    accepts_pointer_button,
    rejects_pointer_button,
    record(2, 1, 0x110, 0, 7),
    record(2, 1, 0x115, 0, 7),
    Command::PointerButton { .. }
);
browser_record_tests!(
    accepts_pointer_scroll,
    rejects_pointer_scroll,
    record(3, 0, 1.5_f32.to_bits(), (-2.25_f32).to_bits(), 7),
    record(3, 0, f32::NAN.to_bits(), 0, 7),
    Command::PointerScroll { .. }
);
browser_record_tests!(
    accepts_keyboard_key,
    rejects_keyboard_key,
    record(4, 2, 30, 0, 7),
    record(4, 2, 256, 0, 7),
    Command::KeyboardKey { .. }
);
browser_record_tests!(
    accepts_release_all,
    rejects_release_all,
    record(5, 0, 0, 0, 0),
    record(5, 0, 1, 0, 0),
    Command::ReleaseAll
);
browser_record_tests!(
    accepts_resize,
    rejects_resize,
    record(
        6,
        0,
        1280,
        720,
        u32::from(180_u16) | (u32::from(9_u16) << 16)
    ),
    record(
        6,
        0,
        1281,
        720,
        u32::from(180_u16) | (u32::from(9_u16) << 16)
    ),
    Command::Resize { .. }
);
browser_record_tests!(
    accepts_pointer_relative,
    rejects_pointer_relative,
    record(8, 0, 1.5_f32.to_bits(), (-2.25_f32).to_bits(), 7),
    record(8, 0, 0, 0, 0),
    Command::PointerRelative { .. }
);

macro_rules! private_kind_rejection_test {
    ($name:ident, $kind:literal) => {
        #[test]
        fn $name() {
            assert!(parse_browser_record(&record($kind, 0, 0, 0, 0)).is_err());
        }
    };
}

private_kind_rejection_test!(rejects_private_clipboard_kind, 7);
private_kind_rejection_test!(rejects_private_quality_kind, 9);
private_kind_rejection_test!(rejects_private_text_kind, 10);
private_kind_rejection_test!(rejects_private_keyframe_readiness_kind, 11);

#[test]
fn feedback_is_parsed_and_validated_in_one_step() {
    let valid = br#"{"type":"feedback","received":40,"presented":42,"queuePeak":2,"queueBusyMs":25,"sampleMs":1000,"dropped":0,"rtt":20}"#;
    assert!(matches!(
        ClientMessage::parse_json(valid),
        Ok(ClientMessage::Feedback(_))
    ));

    let invalid = br#"{"type":"feedback","received":1,"presented":1,"queuePeak":2,"queueBusyMs":0,"sampleMs":0,"dropped":0,"rtt":20}"#;
    let error = ClientMessage::parse_json(invalid).map_err(|error| error.to_string());
    assert_eq!(error, Err("invalid feedback: sampleMs out of range".into()));
}

#[test]
fn video_frame_header_is_exact() {
    let sample = VideoSample {
        data: vec![1, 2].into(),
        key: true,
        discontinuity: true,
        metadata: FrameMetadata {
            generation: value(pipe::Generation::new(4)),
            width: value(pipe::FrameDimension::new(1280)),
            height: value(pipe::FrameDimension::new(720)),
            capture_nanos: 99_000,
            sequence: 17,
            input_sequence: Some(value(InputSequence::new(8))),
            fps: value(Fps::new(60)),
        },
    };
    assert_eq!(
        encode_video_frame(&sample),
        vec![
            2, 1, 3, 0, 17, 0, 0, 0, 0, 0, 0, 0, 99, 0, 0, 0, 0, 0, 0, 0, 4, 0, 0, 0, 0, 5, 208, 2,
            184, 130, 1, 0, 0, 0, 0, 0, 8, 0, 0, 0, 1, 2
        ]
    );
}
