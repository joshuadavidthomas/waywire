pub mod browser;
pub mod pipe;
mod wire;

pub use wire::Decoder;
pub use wire::InvalidValue;
pub use wire::ProtocolError;
pub use wire::Record;

/// Versions every wire format in this crate together: pipe records, browser control records,
/// browser video frames, and the `version` field of the `video-config` message. Any byte-level
/// change to any of them bumps this number.
pub const PROTOCOL_VERSION: u8 = 7;
