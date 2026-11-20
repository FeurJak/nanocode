use std::io;

use super::store::corrupt;

const HEADER: usize = 4;
const FLUSH: usize = 0;
const DELIM: usize = 1;
const RESPONSE_END: usize = 2;

pub enum Pkt<'a> {
    Data(&'a [u8]),
    Flush,
    Delim,
    End,
}

pub fn line(out: &mut Vec<u8>, data: &[u8]) {
    out.extend_from_slice(format!("{:04x}", data.len() + HEADER).as_bytes());
    out.extend_from_slice(data);
}

pub fn flush(out: &mut Vec<u8>) {
    out.extend_from_slice(b"0000");
}

pub struct Reader<'a> {
    data: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    pub fn new(data: &'a [u8]) -> Reader<'a> {
        Reader { data, at: 0 }
    }

    pub fn rest(&self) -> &'a [u8] {
        &self.data[self.at..]
    }

    pub fn next(&mut self) -> io::Result<Pkt<'a>> {
        let rest = self.rest();
        if rest.is_empty() {
            return Ok(Pkt::End);
        }
        let bad = || corrupt("the remote sent a malformed packet line");
        let head = rest.get(..HEADER).ok_or_else(bad)?;
        let head = std::str::from_utf8(head).map_err(|_| bad())?;
        let len = usize::from_str_radix(head, 16).map_err(|_| bad())?;
        match len {
            FLUSH | RESPONSE_END => {
                self.at += HEADER;
                Ok(Pkt::Flush)
            }
            DELIM => {
                self.at += HEADER;
                Ok(Pkt::Delim)
            }
            _ if len < HEADER => Err(bad()),
            _ => {
                let data = rest.get(HEADER..len).ok_or_else(bad)?;
                self.at += len;
                Ok(Pkt::Data(data))
            }
        }
    }

    pub fn text(&mut self) -> io::Result<Option<&'a [u8]>> {
        match self.next()? {
            Pkt::Data(data) => Ok(Some(data.strip_suffix(b"\n").unwrap_or(data))),
            Pkt::Flush | Pkt::Delim | Pkt::End => Ok(None),
        }
    }
}
