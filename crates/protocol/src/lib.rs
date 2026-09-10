pub mod browser;
pub mod pipe;

/// Versions every wire format in this crate together: pipe records, browser control records,
/// browser video frames, and the `version` field of the `video-config` message. Any byte-level
/// change to any of them bumps this number.
pub const PROTOCOL_VERSION: u8 = 2;
