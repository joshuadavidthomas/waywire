//! Browser-to-gateway and gateway-to-browser protocol vocabulary.
//! It covers JSON control messages, 16-byte binary control records that reuse the pipe command
//! header, and binary video frames.

use std::sync::Arc;

use serde::Deserialize;
use serde::Serialize;
use serde::Serializer;
use thiserror::Error;

use crate::PROTOCOL_VERSION;
use crate::pipe;
use crate::pipe::ClipboardText;
use crate::pipe::Command;
use crate::pipe::CommandHeader;
use crate::pipe::CursorVisibility;
use crate::pipe::Fps;
use crate::pipe::FrameMetadata;
use crate::pipe::InputSequence;
use crate::pipe::InputText;
use crate::pipe::Kbps;
use crate::pipe::ScalePercent;
pub use crate::pipe::TextAction;

pub const VIDEO_FRAME_HEADER_BYTES: usize = 40;
const VIDEO_RECORD_KIND: u8 = 1;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum ClientEvent {
    #[serde(rename_all = "camelCase")]
    VideoConfig {
        version: u8,
        codec: String,
        frame_rate: Fps,
    },
    Cursor(CursorState),
    Clipboard {
        text: ClipboardText,
    },
    ResizeApplied(ResizeApplied),
    Quality(QualityLevels),
    ControlState {
        state: ControlState,
    },
    #[serde(rename_all = "camelCase")]
    Pong {
        id: u64,
        server_nanos: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ResizeApplied {
    pub request: pipe::RequestId,
    #[serde(flatten)]
    pub size: pipe::FrameSize,
    pub scale: pipe::ScaleV120,
    pub generation: pipe::Generation,
}

impl ClientEvent {
    #[must_use]
    pub fn video_config(codec: String, frame_rate: Fps) -> Self {
        Self::VideoConfig {
            version: PROTOCOL_VERSION,
            codec,
            frame_rate,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ControlState {
    Active,
    Busy,
    Ready,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CursorState {
    #[serde(rename = "visible")]
    pub visibility: CursorVisibility,
    pub width: u32,
    pub height: u32,
    pub hotspot_x: i32,
    pub hotspot_y: i32,
    pub image: String,
}

impl Serialize for CursorVisibility {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_bool(matches!(self, Self::Visible))
    }
}

impl Default for CursorState {
    fn default() -> Self {
        Self {
            visibility: CursorVisibility::Visible,
            width: 0,
            height: 0,
            hotspot_x: 0,
            hotspot_y: 0,
            image: String::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct QualityLevels {
    pub bitrate: Kbps,
    pub fps: Fps,
    pub scale: ScalePercent,
}

#[derive(Debug, PartialEq)]
pub enum ClientMessage {
    Ping {
        id: u64,
    },
    Feedback(Feedback),
    Text {
        action: TextAction,
        text: InputText,
        sequence: InputSequence,
    },
    ClipboardWrite {
        text: ClipboardText,
    },
}

impl ClientMessage {
    pub fn parse_json(bytes: &[u8]) -> Result<Self, BrowserError> {
        let input: JsonInput = serde_json::from_slice(bytes).map_err(BrowserError::InvalidJson)?;
        match input {
            JsonInput::Ping { id } => Ok(Self::Ping { id }),
            JsonInput::Feedback(values) => Feedback::new(values).map(Self::Feedback),
            JsonInput::Text {
                action,
                text,
                sequence,
            } => Ok(Self::Text {
                action,
                text: InputText::new(text).map_err(BrowserError::InvalidText)?,
                sequence: InputSequence::new(sequence).map_err(BrowserError::InvalidSequence)?,
            }),
            JsonInput::ClipboardWrite { text } => Ok(Self::ClipboardWrite {
                text: ClipboardText::new(text).map_err(BrowserError::InvalidClipboard)?,
            }),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
enum JsonInput {
    Ping {
        id: u64,
    },
    Feedback(FeedbackValues),
    Text {
        action: TextAction,
        text: String,
        sequence: u32,
    },
    ClipboardWrite {
        text: String,
    },
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FeedbackValues {
    pub received: u32,
    pub presented: u32,
    pub queue_peak: u32,
    pub queue_busy_ms: f64,
    pub sample_ms: f64,
    pub dropped: u32,
    pub rtt: f64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Feedback(FeedbackValues);

impl Feedback {
    pub fn new(values: FeedbackValues) -> Result<Self, BrowserError> {
        if values.received > 10_000 {
            return Err(BrowserError::InvalidFeedback("received out of range"));
        }
        if values.presented > 10_000 {
            return Err(BrowserError::InvalidFeedback("presented out of range"));
        }
        if values.queue_peak > 64 {
            return Err(BrowserError::InvalidFeedback("queuePeak out of range"));
        }
        if !values.sample_ms.is_finite()
            || !(0.0..=60_000.0).contains(&values.sample_ms)
            || values.sample_ms == 0.0
        {
            return Err(BrowserError::InvalidFeedback("sampleMs out of range"));
        }
        if !values.queue_busy_ms.is_finite()
            || values.queue_busy_ms < 0.0
            || values.queue_busy_ms > values.sample_ms
        {
            return Err(BrowserError::InvalidFeedback("queueBusyMs out of range"));
        }
        if values.dropped > 10_000 {
            return Err(BrowserError::InvalidFeedback("dropped out of range"));
        }
        if !values.rtt.is_finite() || !(0.0..=60_000.0).contains(&values.rtt) {
            return Err(BrowserError::InvalidFeedback("rtt out of range"));
        }
        Ok(Self(values))
    }

    #[must_use]
    pub const fn received(self) -> u32 {
        self.0.received
    }

    #[must_use]
    pub const fn presented(self) -> u32 {
        self.0.presented
    }

    #[must_use]
    pub const fn queue_peak(self) -> u32 {
        self.0.queue_peak
    }

    #[must_use]
    pub const fn queue_busy_ms(self) -> f64 {
        self.0.queue_busy_ms
    }

    #[must_use]
    pub const fn sample_ms(self) -> f64 {
        self.0.sample_ms
    }

    #[must_use]
    pub const fn dropped(self) -> u32 {
        self.0.dropped
    }

    #[must_use]
    pub const fn rtt(self) -> f64 {
        self.0.rtt
    }
}

#[derive(Debug, Error)]
pub enum BrowserError {
    #[error("invalid browser control header")]
    InvalidControlHeader(#[source] pipe::ProtocolError),
    #[error("browser may not send control kind {0}")]
    PrivateControlKind(u8),
    #[error("invalid browser control fields for kind {kind}")]
    InvalidControlFields {
        kind: u8,
        #[source]
        source: pipe::ProtocolError,
    },
    #[error("invalid JSON control")]
    InvalidJson(#[source] serde_json::Error),
    #[error("invalid feedback: {0}")]
    InvalidFeedback(&'static str),
    #[error("invalid text")]
    InvalidText(#[source] pipe::InvalidValue),
    #[error("invalid input sequence")]
    InvalidSequence(#[source] pipe::InvalidValue),
    #[error("invalid clipboard")]
    InvalidClipboard(#[source] pipe::InvalidValue),
}

pub fn parse_browser_record(bytes: &[u8]) -> Result<Command, BrowserError> {
    let header = CommandHeader::parse(bytes).map_err(BrowserError::InvalidControlHeader)?;
    let kind = header.wire_kind();
    if !header.is_browser_input() {
        return Err(BrowserError::PrivateControlKind(kind));
    }
    Command::decode(header, &[])
        .map_err(|source| BrowserError::InvalidControlFields { kind, source })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameKind {
    Delta,
    Key,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Continuity {
    Continuous,
    AfterGap,
}

#[derive(Clone, Debug)]
pub struct VideoSample {
    pub data: Arc<[u8]>,
    pub kind: FrameKind,
    pub continuity: Continuity,
    pub metadata: FrameMetadata,
}

impl VideoSample {
    /// Encodes a `VIDEO_FRAME_HEADER_BYTES`-byte header: version at 0, `VIDEO_RECORD_KIND` at 1,
    /// flags at 2, zero at 3, sequence at 4, capture time in microseconds at 12, generation at 20,
    /// width at 24, height at 26, capture time in nanoseconds at 28, and input sequence at 36.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        let metadata = &self.metadata;
        let flags = match self.kind {
            FrameKind::Delta => 0,
            FrameKind::Key => 1,
        } | match self.continuity {
            Continuity::Continuous => 0,
            Continuity::AfterGap => 2,
        };
        let mut bytes = Vec::with_capacity(VIDEO_FRAME_HEADER_BYTES + self.data.len());
        bytes.extend_from_slice(&[PROTOCOL_VERSION, VIDEO_RECORD_KIND, flags, 0]);
        bytes.extend_from_slice(&metadata.sequence.to_le_bytes());
        bytes.extend_from_slice(&(metadata.capture_nanos / 1_000).to_le_bytes());
        bytes.extend_from_slice(&metadata.generation.get().to_le_bytes());
        bytes.extend_from_slice(&metadata.width.get().to_le_bytes());
        bytes.extend_from_slice(&metadata.height.get().to_le_bytes());
        bytes.extend_from_slice(&metadata.capture_nanos.to_le_bytes());
        bytes.extend_from_slice(
            &metadata
                .input_sequence
                .map_or(0, InputSequence::get)
                .to_le_bytes(),
        );
        bytes.extend_from_slice(&self.data);
        bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn value<T>(result: Result<T, pipe::InvalidValue>) -> T {
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
            r#"{"type":"video-config","version":2,"codec":"avc1.F40034","frameRate":60}"#
        );
    }

    #[test]
    fn cursor_json_is_exact() {
        let cursor = CursorState {
            visibility: CursorVisibility::Visible,
            width: 1,
            height: 2,
            hotspot_x: -3,
            hotspot_y: 4,
            image: "data:image/png;base64,AA==".into(),
        };
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
            r#"{"type":"clipboard","text":"hello"}"#
        );
    }

    #[test]
    fn resize_applied_json_is_exact() {
        let event = ClientEvent::ResizeApplied(ResizeApplied {
            request: value(pipe::RequestId::new(9)),
            size: value(pipe::FrameSize::new(1280, 720)),
            scale: value(pipe::ScaleV120::new(180)),
            generation: value(pipe::Generation::new(4)),
        });
        assert_eq!(
            json(&event),
            r#"{"type":"resize-applied","request":9,"width":1280,"height":720,"scale":180,"generation":4}"#
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
            r#"{"type":"quality","bitrate":8000,"fps":60,"scale":75}"#
        );
    }

    #[test]
    fn control_state_json_is_exact() {
        assert_eq!(
            json(&ClientEvent::ControlState {
                state: ControlState::Active
            }),
            r#"{"type":"control-state","state":"active"}"#
        );
    }

    #[test]
    fn pong_json_is_exact() {
        assert_eq!(
            json(&ClientEvent::Pong {
                id: 3,
                server_nanos: "99".into()
            }),
            r#"{"type":"pong","id":3,"serverNanos":"99"}"#
        );
    }

    fn browser_record_cases() -> Vec<(&'static str, Vec<u8>, Vec<u8>)> {
        vec![
            (
                "pointer absolute",
                record(1, 0, 12, 34, 7),
                record(1, 1, 12, 34, 7),
            ),
            (
                "pointer button",
                record(2, 1, 0x110, 0, 7),
                record(2, 1, 0x115, 0, 7),
            ),
            (
                "pointer scroll",
                record(3, 0, 1.5_f32.to_bits(), (-2.25_f32).to_bits(), 7),
                record(3, 0, f32::NAN.to_bits(), 0, 7),
            ),
            (
                "keyboard key",
                record(4, 2, 30, 0, 7),
                record(4, 2, 256, 0, 7),
            ),
            ("release all", record(5, 0, 0, 0, 0), record(5, 0, 1, 0, 0)),
            (
                "resize",
                record(
                    6,
                    0,
                    1280,
                    720,
                    u32::from(180_u16) | (u32::from(9_u16) << 16),
                ),
                record(
                    6,
                    0,
                    1281,
                    720,
                    u32::from(180_u16) | (u32::from(9_u16) << 16),
                ),
            ),
            (
                "pointer relative",
                record(8, 0, 1.5_f32.to_bits(), (-2.25_f32).to_bits(), 7),
                record(8, 0, 0, 0, 0),
            ),
        ]
    }

    #[test]
    fn browser_records_accept_valid_input() {
        for (name, accepted, _) in browser_record_cases() {
            assert!(
                parse_browser_record(&accepted).is_ok(),
                "{name} should be accepted"
            );
        }
    }

    #[test]
    fn browser_records_reject_invalid_input() {
        for (name, _, rejected) in browser_record_cases() {
            assert!(
                parse_browser_record(&rejected).is_err(),
                "{name} should be rejected"
            );
        }
    }

    #[test]
    fn browser_records_reject_private_kinds() {
        for kind in [7, 9, 10, 11] {
            assert!(
                matches!(
                    parse_browser_record(&record(kind, 0, 0, 0, 0)),
                    Err(BrowserError::PrivateControlKind(actual)) if actual == kind
                ),
                "private kind {kind} should be rejected"
            );
        }
    }

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
    fn parses_ping_json() {
        assert_eq!(
            ClientMessage::parse_json(br#"{"type":"ping","id":3}"#).expect("ping should parse"),
            ClientMessage::Ping { id: 3 }
        );
    }

    #[test]
    fn parses_text_json() {
        assert_eq!(
            ClientMessage::parse_json(
                br#"{"type":"text","action":"commit","text":"hello","sequence":7}"#
            )
            .expect("text message should parse"),
            ClientMessage::Text {
                action: TextAction::Commit,
                text: value(InputText::new("hello".into())),
                sequence: value(InputSequence::new(7)),
            }
        );
    }

    #[test]
    fn parses_clipboard_write_json() {
        assert_eq!(
            ClientMessage::parse_json(br#"{"type":"clipboard-write","text":"hello"}"#)
                .expect("clipboard write should parse"),
            ClientMessage::ClipboardWrite {
                text: value(ClipboardText::new("hello".into())),
            }
        );
    }

    #[test]
    fn rejects_zero_text_sequence() {
        assert!(matches!(
            ClientMessage::parse_json(
                br#"{"type":"text","action":"commit","text":"hello","sequence":0}"#
            ),
            Err(BrowserError::InvalidSequence(_))
        ));
    }

    #[test]
    fn video_frame_header_is_exact() {
        let sample = VideoSample {
            data: vec![1, 2].into(),
            kind: FrameKind::Key,
            continuity: Continuity::AfterGap,
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
            sample.encode(),
            vec![
                2, 1, 3, 0, 17, 0, 0, 0, 0, 0, 0, 0, 99, 0, 0, 0, 0, 0, 0, 0, 4, 0, 0, 0, 0, 5,
                208, 2, 184, 130, 1, 0, 0, 0, 0, 0, 8, 0, 0, 0, 1, 2
            ]
        );
    }
}
