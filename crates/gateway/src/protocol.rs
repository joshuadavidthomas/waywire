use std::io;

use serde::Deserialize;
use thiserror::Error;
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    time::{Duration, timeout},
};

const PARTIAL_EVENT_DEADLINE: Duration = Duration::from_secs(2);

pub const VERSION: u8 = 2;
pub const CONTROL_BYTES: usize = 16;
pub const MAX_CLIPBOARD_BYTES: usize = 1 << 20;
pub const MAX_TEXT_BYTES: usize = 4_000;
pub const MAX_EVENT_BYTES: usize = MAX_CLIPBOARD_BYTES;
pub const MAX_CURSOR_DIMENSION: u32 = 256;

#[derive(Debug, Error)]
pub enum ProtocolError {
    #[error("peer output ended halfway through an event")]
    Truncated,
    #[error("invalid event header")]
    InvalidHeader,
    #[error("invalid event type {0} payload")]
    InvalidEvent(u8),
    #[error("event exceeds the byte limit")]
    TooLarge,
    #[error("invalid UTF-8")]
    Utf8,
    #[error("event pipe failed")]
    Io(#[source] io::Error),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrameMetadata {
    pub generation: u32,
    pub width: u16,
    pub height: u16,
    pub capture_nanos: u64,
    pub sequence: u64,
    pub input_sequence: u32,
    pub fps: u32,
}

#[derive(Debug, PartialEq, Eq)]
pub enum DaemonEvent {
    Clipboard(String),
    Frame(FrameMetadata),
    ResizeApplied {
        request: u16,
        width: u32,
        height: u32,
        scale: u16,
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

pub struct EventReader<R> {
    input: R,
}
impl<R: AsyncRead + Unpin> EventReader<R> {
    pub fn new(input: R) -> Self {
        Self { input }
    }

    pub async fn next(&mut self) -> Result<Option<DaemonEvent>, ProtocolError> {
        let mut header = [0; 8];
        let first = self
            .input
            .read(&mut header[..1])
            .await
            .map_err(ProtocolError::Io)?;
        if first == 0 {
            return Ok(None);
        }
        timeout(
            PARTIAL_EVENT_DEADLINE,
            self.input.read_exact(&mut header[1..]),
        )
        .await
        .map_err(|_| ProtocolError::Truncated)?
        .map_err(map_eof)?;
        if header[0] != VERSION || header[2] != 0 || header[3] != 0 {
            return Err(ProtocolError::InvalidHeader);
        }
        let kind = header[1];
        let len = u32::from_le_bytes(header[4..8].try_into().unwrap()) as usize;
        if len > MAX_EVENT_BYTES {
            return Err(ProtocolError::TooLarge);
        }
        let mut payload = vec![0; len];
        timeout(PARTIAL_EVENT_DEADLINE, self.input.read_exact(&mut payload))
            .await
            .map_err(|_| ProtocolError::Truncated)?
            .map_err(map_eof)?;
        decode_event(kind, payload).map(Some)
    }
}

fn map_eof(error: io::Error) -> ProtocolError {
    if error.kind() == io::ErrorKind::UnexpectedEof {
        ProtocolError::Truncated
    } else {
        ProtocolError::Io(error)
    }
}

fn decode_event(kind: u8, payload: Vec<u8>) -> Result<DaemonEvent, ProtocolError> {
    let invalid = || ProtocolError::InvalidEvent(kind);
    match kind {
        1 => Ok(DaemonEvent::Clipboard(
            String::from_utf8(payload).map_err(|_| ProtocolError::Utf8)?,
        )),
        2 if payload.len() == 32 => {
            let frame = FrameMetadata {
                generation: le32(&payload[0..4]),
                width: le16(&payload[4..6]),
                height: le16(&payload[6..8]),
                capture_nanos: le64(&payload[8..16]),
                sequence: le64(&payload[16..24]),
                input_sequence: le32(&payload[24..28]),
                fps: le32(&payload[28..32]),
            };
            if frame.generation == 0
                || frame.width == 0
                || frame.height == 0
                || !(10..=120).contains(&frame.fps)
            {
                return Err(invalid());
            }
            Ok(DaemonEvent::Frame(frame))
        }
        3 if payload.len() == 20 && payload[2..4] == [0, 0] && payload[14..16] == [0, 0] => {
            let request = le16(&payload[0..2]);
            let width = le32(&payload[4..8]);
            let height = le32(&payload[8..12]);
            let scale = le16(&payload[12..14]);
            let generation = le32(&payload[16..20]);
            if request == 0
                || generation == 0
                || !valid_size(width, height)
                || !(120..=480).contains(&scale)
            {
                return Err(invalid());
            }
            Ok(DaemonEvent::ResizeApplied {
                request,
                width,
                height,
                scale,
                generation,
            })
        }
        4 if payload.len() >= 16 => {
            let width = le32(&payload[0..4]);
            let height = le32(&payload[4..8]);
            let bytes = cursor_bytes(width, height).ok_or_else(invalid)?;
            if payload.len() != 16 + bytes {
                return Err(invalid());
            }
            Ok(DaemonEvent::CursorImage {
                width,
                height,
                hotspot_x: le32(&payload[8..12]) as i32,
                hotspot_y: le32(&payload[12..16]) as i32,
                bgra: payload[16..].to_vec(),
            })
        }
        5 if payload.as_slice() == [0] || payload.as_slice() == [1] => {
            Ok(DaemonEvent::CursorVisibility(payload[0] == 1))
        }
        _ => Err(invalid()),
    }
}

fn cursor_bytes(width: u32, height: u32) -> Option<usize> {
    if width == 0 || height == 0 || width > MAX_CURSOR_DIMENSION || height > MAX_CURSOR_DIMENSION {
        return None;
    }
    usize::try_from(u64::from(width) * u64::from(height) * 4).ok()
}
fn le16(b: &[u8]) -> u16 {
    u16::from_le_bytes(b.try_into().unwrap())
}
fn le32(b: &[u8]) -> u32 {
    u32::from_le_bytes(b.try_into().unwrap())
}
fn le64(b: &[u8]) -> u64 {
    u64::from_le_bytes(b.try_into().unwrap())
}

#[derive(Clone, Debug, PartialEq)]
pub struct BrowserCommand {
    kind: u8,
    state: u8,
    a: u32,
    b: u32,
    c: u32,
}

pub fn parse_browser_record(bytes: &[u8]) -> Result<BrowserCommand, &'static str> {
    if bytes.len() != CONTROL_BYTES || bytes[0] != VERSION || bytes[3] != 0 || bytes[2] > 2 {
        return Err("invalid browser control header");
    }
    let value = BrowserCommand {
        kind: bytes[1],
        state: bytes[2],
        a: le32(&bytes[4..8]),
        b: le32(&bytes[8..12]),
        c: le32(&bytes[12..16]),
    };
    let valid = match value.kind {
        1 => value.state == 0 && value.a <= 65_535 && value.b <= 65_535 && value.c != 0,
        2 => value.state <= 1 && (0x110..=0x114).contains(&value.a) && value.b == 0 && value.c != 0,
        3 | 8 => {
            value.state == 0 && bounded_float(value.a) && bounded_float(value.b) && value.c != 0
        }
        4 => value.state <= 2 && (1..256).contains(&value.a) && value.b == 0 && value.c != 0,
        5 => value.state == 0 && value.a == 0 && value.b == 0 && value.c == 0,
        6 => {
            value.state == 0
                && valid_size(value.a, value.b)
                && (120..=480).contains(&(value.c as u16))
                && value.c >> 16 != 0
        }
        _ => false,
    };
    valid
        .then_some(value)
        .ok_or("invalid browser control fields")
}
fn bounded_float(bits: u32) -> bool {
    let value = f32::from_bits(bits);
    value.is_finite() && value.abs() <= 4096.0
}
fn valid_size(width: u32, height: u32) -> bool {
    (320..=6000).contains(&width)
        && (180..=6000).contains(&height)
        && width.is_multiple_of(2)
        && height.is_multiple_of(2)
        && u64::from(width) * u64::from(height) <= 3840 * 2160
}

#[derive(Clone, Debug, PartialEq)]
pub enum DaemonCommand {
    Browser(BrowserCommand),
    ReleaseAll,
    KeyframeReadiness {
        generation: u32,
        ready: bool,
    },
    Quality {
        bitrate: u32,
        fps: u32,
        scale: u32,
    },
    Clipboard(String),
    Text {
        preedit: bool,
        text: String,
        sequence: u32,
    },
}
impl DaemonCommand {
    pub fn encoded_len(&self) -> usize {
        CONTROL_BYTES
            + match self {
                Self::Clipboard(text) | Self::Text { text, .. } => text.len(),
                _ => 0,
            }
    }
    pub fn encode(self) -> Vec<u8> {
        match self {
            Self::Browser(v) => command(v.kind, v.state, v.a, v.b, v.c),
            Self::ReleaseAll => command(5, 0, 0, 0, 0),
            Self::KeyframeReadiness { generation, ready } => {
                command(11, u8::from(ready), generation, 0, 0)
            }
            Self::Quality {
                bitrate,
                fps,
                scale,
            } => command(9, 0, bitrate, fps, scale),
            Self::Clipboard(text) => {
                let mut out = command(7, 0, text.len() as u32, 0, 0);
                out.extend_from_slice(text.as_bytes());
                out
            }
            Self::Text {
                preedit,
                text,
                sequence,
            } => {
                let mut out = command(10, u8::from(preedit), text.len() as u32, sequence, 0);
                out.extend_from_slice(text.as_bytes());
                out
            }
        }
    }
}
fn command(kind: u8, state: u8, a: u32, b: u32, c: u32) -> Vec<u8> {
    let mut out = vec![VERSION, kind, state, 0];
    out.extend(a.to_le_bytes());
    out.extend(b.to_le_bytes());
    out.extend(c.to_le_bytes());
    out
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Feedback {
    pub received: u32,
    pub presented: u32,
    #[serde(rename = "queuePeak")]
    pub queue_peak: u32,
    #[serde(rename = "queueBusyMs")]
    pub queue_busy_ms: f64,
    #[serde(rename = "sampleMs")]
    pub sample_ms: f64,
    pub dropped: u32,
    pub rtt: f64,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
pub enum JsonInput {
    #[serde(rename = "ping")]
    Ping { id: u64 },
    #[serde(rename = "feedback")]
    Feedback(Feedback),
    #[serde(rename = "text")]
    Text {
        action: TextAction,
        text: String,
        sequence: u32,
    },
    #[serde(rename = "clipboard-write")]
    ClipboardWrite { text: String },
}
#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TextAction {
    Preedit,
    Commit,
}

pub fn parse_json(bytes: &[u8]) -> Result<JsonInput, &'static str> {
    let value: JsonInput = serde_json::from_slice(bytes).map_err(|_| "invalid JSON control")?;
    match &value {
        JsonInput::Feedback(feedback)
            if feedback.received > 10_000
                || feedback.presented > 10_000
                || feedback.queue_peak > 64
                || feedback.dropped > 10_000
                || !feedback.queue_busy_ms.is_finite()
                || feedback.queue_busy_ms < 0.0
                || !feedback.sample_ms.is_finite()
                || !(0.0..=60_000.0).contains(&feedback.sample_ms)
                || feedback.sample_ms == 0.0
                || feedback.queue_busy_ms > feedback.sample_ms
                || !feedback.rtt.is_finite()
                || !(0.0..=60_000.0).contains(&feedback.rtt) =>
        {
            Err("invalid feedback")
        }
        JsonInput::Text { text, sequence, .. }
            if text.len() > MAX_TEXT_BYTES || text.contains('\0') || *sequence == 0 =>
        {
            Err("invalid text")
        }
        JsonInput::ClipboardWrite { text } if text.len() > MAX_CLIPBOARD_BYTES => {
            Err("invalid clipboard")
        }
        _ => Ok(value),
    }
}

#[derive(Clone, Debug)]
pub struct VideoSample {
    pub data: std::sync::Arc<[u8]>,
    pub key: bool,
    pub discontinuity: bool,
    pub metadata: FrameMetadata,
}
pub fn encode_video(sample: &VideoSample) -> Vec<u8> {
    let m = &sample.metadata;
    let mut out = vec![0; 40 + sample.data.len()];
    out[0] = VERSION;
    out[1] = 1;
    out[2] = u8::from(sample.key) | (u8::from(sample.discontinuity) << 1);
    out[4..12].copy_from_slice(&m.sequence.to_le_bytes());
    out[12..20].copy_from_slice(&(m.capture_nanos / 1000).to_le_bytes());
    out[20..24].copy_from_slice(&m.generation.to_le_bytes());
    out[24..26].copy_from_slice(&m.width.to_le_bytes());
    out[26..28].copy_from_slice(&m.height.to_le_bytes());
    out[28..36].copy_from_slice(&m.capture_nanos.to_le_bytes());
    out[36..40].copy_from_slice(&m.input_sequence.to_le_bytes());
    out[40..].copy_from_slice(&sample.data);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;
    fn record(kind: u8, state: u8, a: u32, b: u32, c: u32) -> Vec<u8> {
        command(kind, state, a, b, c)
    }
    #[test]
    fn browser_rejects_private_and_nonfinite() {
        assert!(
            parse_browser_record(
                &DaemonCommand::KeyframeReadiness {
                    generation: 4,
                    ready: true
                }
                .encode()
            )
            .is_err()
        );
        assert!(parse_browser_record(&record(8, 0, f32::NAN.to_bits(), 0, 1)).is_err());
        assert!(parse_browser_record(&record(4, 2, 30, 0, 7)).is_ok());
    }
    #[test]
    fn feedback_requires_current_fields_and_rejects_unknown_fields() {
        assert!(matches!(
            parse_json(br#"{"type":"feedback","received":40,"presented":42,"queuePeak":2,"queueBusyMs":25,"sampleMs":1000,"dropped":0,"rtt":20}"#),
            Ok(JsonInput::Feedback(_))
        ));
        assert!(
            parse_json(
                br#"{"type":"feedback","queue":0,"dropped":0,"fps":60,"rtt":20,"active":true}"#
            )
            .is_err()
        );
        assert!(parse_json(br#"{"type":"feedback","received":1,"presented":1,"queuePeak":2,"dropped":0,"rtt":20}"#).is_err());
        assert!(parse_json(br#"{"type":"feedback","received":1,"presented":1,"queuePeak":65,"queueBusyMs":0,"sampleMs":1000,"dropped":0,"rtt":20}"#).is_err());
        assert!(parse_json(br#"{"type":"feedback","received":1,"presented":1,"queuePeak":2,"queueBusyMs":101,"sampleMs":100,"dropped":0,"rtt":20}"#).is_err());
        assert!(parse_json(br#"{"type":"feedback","received":1,"presented":1,"queuePeak":2,"queueBusyMs":0,"sampleMs":0,"dropped":0,"rtt":20}"#).is_err());
        assert!(parse_json(br#"{"type":"feedback","received":1,"presented":1,"queuePeak":2,"queueBusyMs":0,"sampleMs":60001,"dropped":0,"rtt":20}"#).is_err());
    }

    #[test]
    fn video_header_is_exact_reference_shape() {
        let sample = VideoSample {
            data: vec![1, 2].into(),
            key: true,
            discontinuity: true,
            metadata: FrameMetadata {
                generation: 4,
                width: 1280,
                height: 720,
                capture_nanos: 99_000,
                sequence: 17,
                input_sequence: 8,
                fps: 60,
            },
        };
        let got = encode_video(&sample);
        assert_eq!(got.len(), 42);
        assert_eq!(&got[..4], &[2, 1, 3, 0]);
        assert_eq!(le64(&got[4..12]), 17);
        assert_eq!(le64(&got[12..20]), 99);
        assert_eq!(le32(&got[36..40]), 8);
    }
    #[tokio::test]
    async fn fragmented_events_and_truncated_eof() {
        let (mut tx, rx) = tokio::io::duplex(64);
        tokio::spawn(async move {
            for byte in [2, 5, 0, 0, 1, 0, 0, 0, 1] {
                tx.write_all(&[byte]).await.unwrap();
            }
        });
        let mut reader = EventReader::new(rx);
        assert_eq!(
            reader.next().await.unwrap(),
            Some(DaemonEvent::CursorVisibility(true))
        );
        let (mut tx, rx) = tokio::io::duplex(64);
        tx.write_all(&[2, 4]).await.unwrap();
        drop(tx);
        assert!(matches!(
            EventReader::new(rx).next().await,
            Err(ProtocolError::Truncated)
        ));
    }
    #[test]
    fn rejects_bad_cursor_bounds() {
        assert!(decode_event(4, vec![0; 16]).is_err());
        let mut p = vec![0; 16];
        p[..4].copy_from_slice(&257u32.to_le_bytes());
        p[4..8].copy_from_slice(&1u32.to_le_bytes());
        assert!(decode_event(4, p).is_err());
    }
}
