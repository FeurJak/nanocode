use std::io;

use nanocode_executor::TcpStream;
use nanocode_io::{AsyncReadExt, AsyncWriteExt};

pub const HEAD_BYTES_MAX: usize = 8 * 1024;
pub const BODY_BYTES_MAX: usize = 64 * 1024 * 1024;
const READ_BYTES: usize = 64 * 1024;
const FORWARDED: &str = "x-forwarded";

pub struct Request {
    pub method: String,
    pub path: String,
    pub forwarded: bool,
    pub body: Vec<u8>,
}

pub struct Response {
    pub status: u16,
    pub body: Vec<u8>,
}

struct Message {
    start: String,
    forwarded: bool,
    body: Vec<u8>,
}

pub struct Conn {
    stream: TcpStream,
    buffer: Vec<u8>,
    filled: usize,
}

fn invalid(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, what.to_owned())
}

fn head_end(bytes: &[u8]) -> Option<usize> {
    bytes.windows(4).position(|window| window == b"\r\n\r\n")
}

const fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        404 => "Not Found",
        409 => "Conflict",
        503 => "Service Unavailable",
        _ => "Internal Server Error",
    }
}

impl Conn {
    pub fn new(stream: TcpStream) -> io::Result<Self> {
        stream.set_nodelay(true)?;
        Ok(Self {
            stream,
            buffer: vec![0; READ_BYTES],
            filled: 0,
        })
    }

    pub async fn connect(address: &str) -> io::Result<Self> {
        Self::new(TcpStream::connect(address).await?)
    }

    async fn fill(&mut self) -> io::Result<usize> {
        if self.filled == self.buffer.len() {
            if self.buffer.len() >= HEAD_BYTES_MAX + BODY_BYTES_MAX {
                return Err(invalid("message over HEAD_BYTES_MAX + BODY_BYTES_MAX"));
            }
            self.buffer.resize(self.buffer.len() * 2, 0);
        }
        let read = self.stream.read(&mut self.buffer[self.filled..]).await?;
        self.filled += read;
        Ok(read)
    }

    async fn message(&mut self) -> io::Result<Option<Message>> {
        let end = loop {
            if let Some(end) = head_end(&self.buffer[..self.filled]) {
                break end;
            }
            if self.filled > HEAD_BYTES_MAX {
                return Err(invalid("head over HEAD_BYTES_MAX"));
            }
            if self.fill().await? == 0 {
                if self.filled == 0 {
                    return Ok(None);
                }
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
        };
        let head =
            std::str::from_utf8(&self.buffer[..end]).map_err(|_| invalid("head is not UTF-8"))?;
        let mut lines = head.split("\r\n");
        let start = lines.next().unwrap_or_default().to_owned();
        let (mut length, mut forwarded) = (0, false);
        for line in lines {
            let Some((name, value)) = line.split_once(':') else {
                return Err(invalid("header without a colon"));
            };
            let (name, value) = (name.trim().to_ascii_lowercase(), value.trim());
            if name == "content-length" {
                length = value.parse().map_err(|_| invalid("bad content-length"))?;
            } else if name == FORWARDED {
                forwarded = value == "1";
            }
        }
        if length > BODY_BYTES_MAX {
            return Err(invalid("body over BODY_BYTES_MAX"));
        }
        let total = end + 4 + length;
        while self.filled < total {
            if self.buffer.len() < total {
                self.buffer.resize(total.next_power_of_two(), 0);
            }
            if self.fill().await? == 0 {
                return Err(io::ErrorKind::UnexpectedEof.into());
            }
        }
        let body = self.buffer[end + 4..total].to_vec();
        self.buffer.copy_within(total..self.filled, 0);
        self.filled -= total;
        Ok(Some(Message {
            start,
            forwarded,
            body,
        }))
    }

    pub async fn request(&mut self) -> io::Result<Option<Request>> {
        let Some(message) = self.message().await? else {
            return Ok(None);
        };
        let mut parts = message.start.split(' ');
        let (Some(method), Some(path)) = (parts.next(), parts.next()) else {
            return Err(invalid("bad request line"));
        };
        Ok(Some(Request {
            method: method.to_owned(),
            path: path.to_owned(),
            forwarded: message.forwarded,
            body: message.body,
        }))
    }

    pub async fn respond(&mut self, status: u16, body: &[u8]) -> io::Result<()> {
        let head = format!(
            "HTTP/1.1 {status} {}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n",
            reason(status),
            body.len()
        );
        self.write(head.as_bytes(), body).await
    }

    async fn write(&mut self, head: &[u8], body: &[u8]) -> io::Result<()> {
        let mut out = Vec::with_capacity(head.len() + body.len());
        out.extend_from_slice(head);
        out.extend_from_slice(body);
        self.stream.write_all(&out).await
    }

    pub async fn call(
        &mut self,
        method: &str,
        path: &str,
        forwarded: bool,
        body: &[u8],
    ) -> io::Result<Response> {
        let head = format!(
            "{method} {path} HTTP/1.1\r\nhost: scoped-api\r\n{FORWARDED}: {}\r\ncontent-length: {}\r\n\r\n",
            u8::from(forwarded),
            body.len()
        );
        self.write(head.as_bytes(), body).await?;
        let Some(message) = self.message().await? else {
            return Err(io::ErrorKind::UnexpectedEof.into());
        };
        let status = message
            .start
            .split(' ')
            .nth(1)
            .and_then(|status| status.parse().ok())
            .ok_or_else(|| invalid("bad status line"))?;
        Ok(Response {
            status,
            body: message.body,
        })
    }
}
