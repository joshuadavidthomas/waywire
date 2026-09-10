use sprite_desktop_protocol::pipe::COMMAND_HEADER_BYTES;
use sprite_desktop_protocol::pipe::Command;
use sprite_desktop_protocol::pipe::CommandHeader;
use sprite_desktop_protocol::pipe::MAX_CLIPBOARD_BYTES;
use sprite_desktop_protocol::pipe::ProtocolError;

pub(crate) struct CommandReader {
    bytes: Vec<u8>,
}

impl CommandReader {
    pub(crate) fn new() -> Self {
        Self { bytes: Vec::new() }
    }

    pub(crate) fn push(&mut self, incoming: &[u8]) -> Result<Vec<Command>, ProtocolError> {
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
            let header = CommandHeader::parse(&self.bytes[..COMMAND_HEADER_BYTES])?;
            let record_len = match header.text_payload_len() {
                Some(text_payload_len) => COMMAND_HEADER_BYTES + text_payload_len,
                None => COMMAND_HEADER_BYTES,
            };
            if self.bytes.len() < record_len {
                break;
            }
            let text = &self.bytes[COMMAND_HEADER_BYTES..record_len];
            commands.push(Command::decode(header, text)?);
            let _ = self.bytes.drain(..record_len);
        }
        Ok(commands)
    }

    pub(crate) fn finish(self) -> Result<(), ProtocolError> {
        if self.bytes.is_empty() {
            Ok(())
        } else {
            Err(ProtocolError::Truncated)
        }
    }
}

#[cfg(test)]
mod tests {
    use sprite_desktop_protocol::pipe::ClipboardText;

    use super::*;

    #[test]
    fn empty_text_payload_command_does_not_consume_the_next_header() {
        let clipboard = Command::Clipboard(
            ClipboardText::new(String::new()).expect("empty clipboard text should be valid"),
        );
        let release = Command::ReleaseAll;
        let bytes = [clipboard.encode(), release.encode()].concat();
        let mut reader = CommandReader::new();

        assert_eq!(
            reader
                .push(&bytes)
                .expect("two valid commands should parse"),
            vec![clipboard, release]
        );
        assert!(reader.finish().is_ok());
    }
}
