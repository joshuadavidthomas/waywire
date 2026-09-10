use std::marker::PhantomData;

use thiserror::Error;

use crate::PROTOCOL_VERSION;

pub const HEADER_BYTES: usize = 8;

#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
#[error("{0}")]
pub struct InvalidValue(pub(crate) &'static str);

pub(crate) trait Wire: Sized {
    fn write(&self, out: &mut Writer);
    fn read(input: &mut Reader<'_>) -> Result<Self, InvalidValue>;
}

pub struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    pub(crate) const fn new(bytes: &'a [u8]) -> Self {
        Self(bytes)
    }

    fn take<const N: usize>(&mut self) -> Result<[u8; N], InvalidValue> {
        let (head, tail) = self
            .0
            .split_first_chunk::<N>()
            .ok_or(InvalidValue("payload is too short"))?;
        self.0 = tail;
        Ok(*head)
    }

    pub(crate) fn get<T: Wire>(&mut self) -> Result<T, InvalidValue> {
        T::read(self)
    }

    pub(crate) fn rest(&mut self) -> &'a [u8] {
        let rest = self.0;
        self.0 = &[];
        rest
    }

    pub(crate) fn rest_utf8(&mut self) -> Result<&'a str, InvalidValue> {
        let Ok(text) = std::str::from_utf8(self.rest()) else {
            return Err(InvalidValue("text is not UTF-8"));
        };
        Ok(text)
    }

    pub(crate) fn finish(&self) -> Result<(), InvalidValue> {
        if self.0.is_empty() {
            Ok(())
        } else {
            Err(InvalidValue("payload has trailing bytes"))
        }
    }
}

pub struct Writer(Vec<u8>);

impl Writer {
    #[must_use]
    pub(crate) fn with_capacity(capacity: usize) -> Self {
        Self(Vec::with_capacity(capacity))
    }

    pub(crate) fn put<T: Wire>(&mut self, value: &T) {
        value.write(self);
    }

    pub(crate) fn reserved<const N: usize>(&mut self) {
        self.0.extend_from_slice(&[0; N]);
    }

    pub(crate) fn bytes(&mut self, bytes: &[u8]) {
        self.0.extend_from_slice(bytes);
    }

    #[must_use]
    pub(crate) fn into_inner(self) -> Vec<u8> {
        self.0
    }
}

impl Wire for u8 {
    fn write(&self, out: &mut Writer) {
        out.bytes(&self.to_le_bytes());
    }

    fn read(input: &mut Reader<'_>) -> Result<Self, InvalidValue> {
        Ok(Self::from_le_bytes(input.take()?))
    }
}

impl Wire for u16 {
    fn write(&self, out: &mut Writer) {
        out.bytes(&self.to_le_bytes());
    }

    fn read(input: &mut Reader<'_>) -> Result<Self, InvalidValue> {
        Ok(Self::from_le_bytes(input.take()?))
    }
}

impl Wire for u32 {
    fn write(&self, out: &mut Writer) {
        out.bytes(&self.to_le_bytes());
    }

    fn read(input: &mut Reader<'_>) -> Result<Self, InvalidValue> {
        Ok(Self::from_le_bytes(input.take()?))
    }
}

impl Wire for i32 {
    fn write(&self, out: &mut Writer) {
        out.bytes(&self.to_le_bytes());
    }

    fn read(input: &mut Reader<'_>) -> Result<Self, InvalidValue> {
        Ok(Self::from_le_bytes(input.take()?))
    }
}

impl Wire for u64 {
    fn write(&self, out: &mut Writer) {
        out.bytes(&self.to_le_bytes());
    }

    fn read(input: &mut Reader<'_>) -> Result<Self, InvalidValue> {
        Ok(Self::from_le_bytes(input.take()?))
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ProtocolError {
    #[error("stream ended halfway through a record")]
    Truncated,
    #[error("record header is invalid")]
    InvalidHeader,
    #[error("record kind {kind} is unknown")]
    InvalidKind { kind: u8 },
    #[error("record {kind} has an invalid payload: {reason}")]
    InvalidPayload { kind: u8, reason: InvalidValue },
    #[error("record payload exceeds its limit")]
    PayloadTooLarge,
}

pub trait RecordKind: Copy {
    fn wire(self) -> u8;
    fn from_wire(byte: u8) -> Option<Self>;
    /// Largest payload this kind may carry. Fixed-size kinds return their exact size.
    fn max_payload(self) -> usize;
}

#[derive(Clone, Copy)]
struct RecordHeader<K> {
    kind: K,
    payload_len: u32,
}

impl<K: RecordKind> RecordHeader<K> {
    fn write(&self, out: &mut Writer) {
        out.put(&PROTOCOL_VERSION);
        out.put(&self.kind.wire());
        out.reserved::<2>();
        out.put(&self.payload_len);
    }

    /// Reads exactly `HEADER_BYTES` bytes. The version and the two reserved bytes must match.
    fn read(bytes: &[u8]) -> Result<Self, ProtocolError> {
        let &[PROTOCOL_VERSION, kind_byte, 0, 0, l0, l1, l2, l3] = bytes else {
            return Err(ProtocolError::InvalidHeader);
        };
        let Some(kind) = K::from_wire(kind_byte) else {
            return Err(ProtocolError::InvalidKind { kind: kind_byte });
        };
        Ok(Self {
            kind,
            payload_len: u32::from_le_bytes([l0, l1, l2, l3]),
        })
    }

    fn record_len(&self) -> Result<usize, ProtocolError> {
        let Ok(payload_len) = usize::try_from(self.payload_len) else {
            return Err(ProtocolError::PayloadTooLarge);
        };
        if payload_len > self.kind.max_payload() {
            return Err(ProtocolError::PayloadTooLarge);
        }
        HEADER_BYTES
            .checked_add(payload_len)
            .ok_or(ProtocolError::PayloadTooLarge)
    }
}

pub trait Record: Sized {
    type Kind: RecordKind;

    fn kind(&self) -> Self::Kind;
    fn write_payload(&self, out: &mut Writer);
    fn read_payload(kind: Self::Kind, input: &mut Reader<'_>) -> Result<Self, InvalidValue>;

    fn encode(&self) -> Vec<u8> {
        let kind = self.kind();
        let mut payload = Writer::with_capacity(0);
        self.write_payload(&mut payload);
        let payload = payload.into_inner();
        #[expect(
            clippy::cast_possible_truncation,
            reason = "every RecordKind payload limit fits in u32"
        )]
        let payload_len = payload.len() as u32;
        let header = RecordHeader { kind, payload_len };
        let mut out = Writer::with_capacity(HEADER_BYTES + payload.len());
        header.write(&mut out);
        out.bytes(&payload);
        out.into_inner()
    }

    fn record_len(header: &[u8]) -> Result<usize, ProtocolError> {
        RecordHeader::<Self::Kind>::read(header)?.record_len()
    }

    fn decode(record: &[u8]) -> Result<Self, ProtocolError> {
        let Some(header_bytes) = record.get(..HEADER_BYTES) else {
            return Err(ProtocolError::Truncated);
        };
        let header = RecordHeader::<Self::Kind>::read(header_bytes)?;
        let expected_len = header.record_len()?;
        if record.len() < expected_len {
            return Err(ProtocolError::Truncated);
        }
        if record.len() > expected_len {
            return Err(ProtocolError::InvalidHeader);
        }
        let kind = header.kind.wire();
        let mut input = Reader::new(&record[HEADER_BYTES..]);
        Self::read_payload(header.kind, &mut input)
            .and_then(|value| input.finish().map(|()| value))
            .map_err(|reason| ProtocolError::InvalidPayload { kind, reason })
    }
}

pub struct Decoder<R: Record> {
    buffer: Vec<u8>,
    record: PhantomData<R>,
}

impl<R: Record> Decoder<R> {
    #[must_use]
    pub fn new() -> Self {
        Self {
            buffer: Vec::new(),
            record: PhantomData,
        }
    }

    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<R>, ProtocolError> {
        self.buffer.extend_from_slice(bytes);
        let mut records = Vec::new();
        loop {
            if self.buffer.len() < HEADER_BYTES {
                break;
            }
            let record_len = R::record_len(&self.buffer[..HEADER_BYTES])?;
            if self.buffer.len() < record_len {
                break;
            }
            records.push(R::decode(&self.buffer[..record_len])?);
            drop(self.buffer.drain(..record_len));
        }
        Ok(records)
    }

    /// Bytes of the record still being received, zero when none.
    #[must_use]
    pub fn pending(&self) -> usize {
        self.buffer.len()
    }

    /// Errors with `ProtocolError::Truncated` if a record is half received.
    pub fn finish(self) -> Result<(), ProtocolError> {
        if self.buffer.is_empty() {
            Ok(())
        } else {
            Err(ProtocolError::Truncated)
        }
    }
}

impl<R: Record> Default for Decoder<R> {
    fn default() -> Self {
        Self::new()
    }
}
