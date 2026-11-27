use nanocode_compression::checksum::crc32;

pub const PRELUDE_BYTES: usize = 12;
pub const CRC_BYTES: usize = 4;
pub const MESSAGE_BYTES_MIN: usize = PRELUDE_BYTES + CRC_BYTES;
pub const MESSAGE_BYTES_MAX: usize = 16 * 1024 * 1024;
pub const HEADERS_BYTES_MAX: usize = 128 * 1024;
const LENGTH_BYTES: usize = 4;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DecodeError {
    PreludeCrc { expected: u32, actual: u32 },
    MessageCrc { expected: u32, actual: u32 },
    Length { total: u32, headers: u32 },
    Header { offset: u32 },
    Truncated { buffered: u32 },
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PreludeCrc { expected, actual } => write!(
                formatter,
                "event-stream prelude CRC32 is {actual:08x}, the frame says {expected:08x}"
            ),
            Self::MessageCrc { expected, actual } => write!(
                formatter,
                "event-stream message CRC32 is {actual:08x}, the frame says {expected:08x}"
            ),
            Self::Length { total, headers } => write!(
                formatter,
                "event-stream frame of {total} bytes with {headers} header bytes is outside the protocol's limits"
            ),
            Self::Header { offset } => {
                write!(
                    formatter,
                    "event-stream header at byte {offset} is malformed"
                )
            }
            Self::Truncated { buffered } => write!(
                formatter,
                "event stream ended inside a frame with {buffered} bytes buffered"
            ),
        }
    }
}

impl std::error::Error for DecodeError {}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Value {
    Bool(bool),
    Byte(i8),
    Short(i16),
    Int(i32),
    Long(i64),
    Bytes(Vec<u8>),
    String(String),
    Timestamp(i64),
    Uuid([u8; 16]),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Message {
    headers: Vec<(String, Value)>,
    payload: Vec<u8>,
}

impl Message {
    pub fn headers(&self) -> &[(String, Value)] {
        &self.headers
    }

    pub fn payload(&self) -> &[u8] {
        &self.payload
    }

    pub fn text(&self, name: &str) -> Option<&str> {
        self.headers.iter().find_map(|(key, value)| match value {
            Value::String(text) if key == name => Some(text.as_str()),
            _ => None,
        })
    }
}

fn be_u32(bytes: &[u8], at: usize) -> Option<u32> {
    let slice = bytes.get(at..at + LENGTH_BYTES)?;
    Some(u32::from_be_bytes(slice.try_into().ok()?))
}

fn take<'a>(bytes: &'a [u8], at: &mut usize, count: usize) -> Option<&'a [u8]> {
    let slice = bytes.get(*at..at.checked_add(count)?)?;
    *at += count;
    Some(slice)
}

fn fixed<const N: usize>(bytes: &[u8], at: &mut usize) -> Option<[u8; N]> {
    take(bytes, at, N)?.try_into().ok()
}

fn value(bytes: &[u8], at: &mut usize) -> Option<Value> {
    let kind = *take(bytes, at, 1)?.first()?;
    let sized = |at: &mut usize| {
        let length = u16::from_be_bytes(fixed(bytes, at)?);
        take(bytes, at, usize::from(length))
    };
    Some(match kind {
        0 => Value::Bool(true),
        1 => Value::Bool(false),
        2 => Value::Byte(i8::from_be_bytes(fixed(bytes, at)?)),
        3 => Value::Short(i16::from_be_bytes(fixed(bytes, at)?)),
        4 => Value::Int(i32::from_be_bytes(fixed(bytes, at)?)),
        5 => Value::Long(i64::from_be_bytes(fixed(bytes, at)?)),
        6 => Value::Bytes(sized(at)?.to_vec()),
        7 => Value::String(String::from_utf8(sized(at)?.to_vec()).ok()?),
        8 => Value::Timestamp(i64::from_be_bytes(fixed(bytes, at)?)),
        9 => Value::Uuid(fixed(bytes, at)?),
        _ => return None,
    })
}

fn headers(bytes: &[u8]) -> Result<Vec<(String, Value)>, DecodeError> {
    let mut parsed = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        let start = at;
        let malformed = || DecodeError::Header {
            offset: u32::try_from(start + PRELUDE_BYTES).unwrap_or(u32::MAX),
        };
        let length = *take(bytes, &mut at, 1)
            .and_then(<[u8]>::first)
            .ok_or_else(malformed)?;
        let name = take(bytes, &mut at, usize::from(length))
            .and_then(|name| std::str::from_utf8(name).ok())
            .ok_or_else(malformed)?
            .to_owned();
        let value = value(bytes, &mut at).ok_or_else(malformed)?;
        parsed.push((name, value));
    }
    assert_eq!(at, bytes.len());
    Ok(parsed)
}

#[derive(Default)]
pub struct Decoder {
    buffer: Vec<u8>,
}

impl Decoder {
    pub fn push(&mut self, bytes: &[u8]) {
        self.buffer.extend_from_slice(bytes);
    }

    pub fn finish(&self) -> Result<(), DecodeError> {
        match self.buffer.is_empty() {
            true => Ok(()),
            false => Err(DecodeError::Truncated {
                buffered: u32::try_from(self.buffer.len()).unwrap_or(u32::MAX),
            }),
        }
    }

    pub fn next_message(&mut self) -> Result<Option<Message>, DecodeError> {
        let (Some(total), Some(header_bytes), Some(prelude_crc)) = (
            be_u32(&self.buffer, 0),
            be_u32(&self.buffer, LENGTH_BYTES),
            be_u32(&self.buffer, 2 * LENGTH_BYTES),
        ) else {
            return Ok(None);
        };
        let actual = crc32(0, &self.buffer[..2 * LENGTH_BYTES]);
        if actual != prelude_crc {
            return Err(DecodeError::PreludeCrc {
                expected: prelude_crc,
                actual,
            });
        }
        let (length, header_length) = (total as usize, header_bytes as usize);
        if !(MESSAGE_BYTES_MIN..=MESSAGE_BYTES_MAX).contains(&length)
            || header_length > HEADERS_BYTES_MAX
            || header_length > length - MESSAGE_BYTES_MIN
        {
            return Err(DecodeError::Length {
                total,
                headers: header_bytes,
            });
        }
        if self.buffer.len() < length {
            return Ok(None);
        }
        let frame = self.buffer.drain(..length).collect::<Vec<_>>();
        let (body, crc) = frame.split_at(length - CRC_BYTES);
        let expected = u32::from_be_bytes(crc.try_into().unwrap_or_default());
        let actual = crc32(0, body);
        if actual != expected {
            return Err(DecodeError::MessageCrc { expected, actual });
        }
        let (header_slice, payload) = body[PRELUDE_BYTES..].split_at(header_length);
        let message = Message {
            headers: headers(header_slice)?,
            payload: payload.to_vec(),
        };
        assert_eq!(
            message.payload.len() + header_length + MESSAGE_BYTES_MIN,
            length
        );
        Ok(Some(message))
    }
}
