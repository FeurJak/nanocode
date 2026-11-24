use nanocode_compression::{
    deflate::{Deflate, DeflateConfig, DeflateFlush, DeflateStatus, Strategy},
    inflate::{Inflate, InflateFlush, InflateStatus},
};
use nanocode_durability_core::StoreError;
use nanocode_sys::base64::STANDARD_NO_PAD;

pub(crate) const WINDOW: usize = 1 << WINDOW_BITS;
const WINDOW_BITS: i32 = 15;
pub(crate) const DEFAULT_LEVEL: i32 = -1;
pub(crate) const FAST_LEVEL: i32 = 1;
const STREAM_LEVEL: i32 = 7;
const DEFAULT_MEM_LEVEL: i32 = 8;
const FINAL_EMPTY_BLOCK: [u8; 2] = [0x03, 0x00];
const EMPTY_STORED_BLOCK: [u8; 5] = [0x00, 0x00, 0x00, 0xff, 0xff];
const BASE64_GROUP: usize = 3;
const BASE64_CHARS: usize = 4;
pub(crate) const SEAL: usize = (FINAL_EMPTY_BLOCK.len() * BASE64_CHARS).div_ceil(BASE64_GROUP);

pub(crate) const ARCHIVE: char = 'a';
pub(crate) const RECORDS: char = 'r';
pub(crate) const SECTION: char = 'p';
pub(crate) const SNAPSHOT: char = 's';
pub(crate) const PART: char = '+';
pub(crate) const BOUNDARY: char = 'b';
pub(crate) const LINKED: char = 'P';
const VARINT_BITS: u32 = 7;
const VARINT_MORE: u8 = 1 << VARINT_BITS;

#[derive(Clone, Debug, Default)]
struct Window(Vec<u8>);

impl Window {
    fn extend(&mut self, plain: &[u8]) {
        let kept = &plain[plain.len().saturating_sub(WINDOW)..];
        let excess = (self.0.len() + kept.len()).saturating_sub(WINDOW);
        self.0.drain(..excess);
        self.0.extend_from_slice(kept);
    }
}

#[derive(Clone, Debug, Default)]
pub(crate) struct History {
    records: Window,
    chain: Option<(u64, Window)>,
}

impl History {
    pub(crate) fn extend(&mut self, plain: &[u8]) {
        self.records.extend(plain);
    }

    pub(crate) fn clear(&mut self) {
        self.records.0.clear();
        self.chain = None;
    }

    pub(crate) fn bytes(&self) -> &[u8] {
        &self.records.0
    }

    pub(crate) fn keyframe(&self) -> Option<u64> {
        self.chain.as_ref().map(|(keyframe, _)| *keyframe)
    }

    pub(crate) fn chained(&self, keyframe: u64) -> Option<&[u8]> {
        match &self.chain {
            Some((current, window)) if *current == keyframe => Some(&window.0),
            _ => None,
        }
    }

    pub(crate) fn link(&mut self, keyframe: u64, plain: &[u8]) {
        match &mut self.chain {
            Some((current, window)) if *current == keyframe => window.extend(plain),
            _ => {
                let mut window = Window::default();
                window.extend(plain);
                self.chain = Some((keyframe, window));
            }
        }
    }
}

#[cold]
fn failed(action: &str, detail: &str) -> StoreError {
    StoreError::Backend(format!("turn log row {action} failed: {detail}"))
}

fn primed(deflate: &mut Deflate, dictionary: &[u8]) -> Result<(), StoreError> {
    if !dictionary.is_empty() {
        deflate
            .set_dictionary(dictionary)
            .map_err(|error| failed("compression", error.as_str()))?;
    }
    Ok(())
}

fn reused(mut deflate: Deflate, level: i32, dictionary: &[u8]) -> Result<Deflate, StoreError> {
    deflate
        .reset(level)
        .map_err(|error| failed("compression", error.as_str()))?;
    primed(&mut deflate, dictionary)?;
    Ok(deflate)
}

fn compressor(level: i32, dictionary: &[u8]) -> Result<Deflate, StoreError> {
    let mut deflate = Deflate::new(DeflateConfig {
        level,
        mem_level: DEFAULT_MEM_LEVEL,
        strategy: Strategy::Default,
        window_bits: -WINDOW_BITS,
    })
    .map_err(|error| failed("compression", error.as_str()))?;
    primed(&mut deflate, dictionary)?;
    Ok(deflate)
}

fn encode(tag: char, compressed: &[u8]) -> String {
    let mut body = String::with_capacity(1 + compressed.len().div_ceil(3) * 4);
    body.push(tag);
    STANDARD_NO_PAD.encode_string(compressed, &mut body);
    body
}

pub(crate) fn pack(
    spare: &mut Spare,
    tag: char,
    plain: &[u8],
    dictionary: &[u8],
) -> Result<String, StoreError> {
    let mut packer = Packer::new(spare, DEFAULT_LEVEL, dictionary)?;
    packer.write(plain)?;
    Ok(packer.finish(tag, spare)?.packed)
}

#[derive(Default)]
pub(crate) struct Spare {
    chunk: Vec<u8>,
    deflate: Option<Deflate>,
}

pub(crate) struct Packer {
    chunk: Vec<u8>,
    compressed: Vec<u8>,
    deflate: Deflate,
    plain: usize,
}

pub(crate) struct Packed {
    pub(crate) packed: String,
    pub(crate) plain: usize,
}

impl Packer {
    pub(crate) fn new(
        spare: &mut Spare,
        level: i32,
        dictionary: &[u8],
    ) -> Result<Self, StoreError> {
        let deflate = match spare.deflate.take() {
            Some(deflate) => reused(deflate, level, dictionary)?,
            None => compressor(level, dictionary)?,
        };
        let mut chunk = std::mem::take(&mut spare.chunk);
        if chunk.is_empty() {
            chunk = vec![0; WINDOW];
        }
        Ok(Self {
            chunk,
            compressed: Vec::new(),
            deflate,
            plain: 0,
        })
    }

    pub(crate) fn linked(
        spare: &mut Spare,
        dictionary: &[u8],
        back: u64,
    ) -> Result<Self, StoreError> {
        let mut packer = Self::new(spare, DEFAULT_LEVEL, dictionary)?;
        let mut rest = back;
        loop {
            let low =
                u8::try_from(rest & u64::from(VARINT_MORE - 1)).expect("seven bits fit a byte");
            rest >>= VARINT_BITS;
            if rest == 0 {
                packer.compressed.push(low);
                return Ok(packer);
            }
            packer.compressed.push(low | VARINT_MORE);
        }
    }

    pub(crate) const fn plain(&self) -> usize {
        self.plain
    }

    pub(crate) fn write(&mut self, plain: &[u8]) -> Result<(), StoreError> {
        let start = self.deflate.total_in();
        loop {
            let consumed = usize::try_from(self.deflate.total_in() - start)
                .expect("consumed input fits in usize");
            if consumed == plain.len() {
                break;
            }
            let before = self.deflate.total_out();
            self.deflate
                .deflate(&plain[consumed..], &mut self.chunk, DeflateFlush::NoFlush)
                .map_err(|error| failed("compression", error.as_str()))?;
            let produced = usize::try_from(self.deflate.total_out() - before)
                .expect("one call produces at most its chunk");
            self.compressed.extend_from_slice(&self.chunk[..produced]);
            let taken = usize::try_from(self.deflate.total_in() - start)
                .expect("consumed input fits in usize");
            if produced == 0 && taken == consumed {
                return Err(failed("compression", "no progress"));
            }
        }
        self.plain += plain.len();
        Ok(())
    }

    pub(crate) fn finish(mut self, tag: char, spare: &mut Spare) -> Result<Packed, StoreError> {
        loop {
            let before = self.deflate.total_out();
            let status = self
                .deflate
                .deflate(&[], &mut self.chunk, DeflateFlush::Finish)
                .map_err(|error| failed("compression", error.as_str()))?;
            let produced = usize::try_from(self.deflate.total_out() - before)
                .expect("one call produces at most its chunk");
            self.compressed.extend_from_slice(&self.chunk[..produced]);
            match status {
                DeflateStatus::StreamEnd => break,
                DeflateStatus::Ok => {}
                DeflateStatus::BufError if produced > 0 => {}
                DeflateStatus::BufError => return Err(failed("compression", "no progress")),
            }
        }
        let packed = Packed {
            packed: encode(tag, &self.compressed),
            plain: self.plain,
        };
        spare.chunk = self.chunk;
        spare.deflate = Some(self.deflate);
        Ok(packed)
    }
}

pub(crate) struct Stream {
    chunk: Vec<u8>,
    deflate: Deflate,
}

impl Stream {
    pub(crate) fn new(dictionary: &[u8]) -> Result<Self, StoreError> {
        Ok(Self {
            chunk: vec![0; WINDOW],
            deflate: compressor(STREAM_LEVEL, dictionary)?,
        })
    }

    pub(crate) fn pack(&mut self, tag: char, plain: &[u8]) -> Result<String, StoreError> {
        let deflate = &mut self.deflate;
        let chunk = &mut self.chunk;
        let start = deflate.total_in();
        let mut compressed = Vec::new();
        loop {
            let consumed =
                usize::try_from(deflate.total_in() - start).expect("consumed input fits in usize");
            let before = deflate.total_out();
            deflate
                .deflate(&plain[consumed..], chunk, DeflateFlush::SyncFlush)
                .map_err(|error| failed("compression", error.as_str()))?;
            let produced = usize::try_from(deflate.total_out() - before)
                .expect("one call produces at most its chunk");
            compressed.extend_from_slice(&chunk[..produced]);
            let taken =
                usize::try_from(deflate.total_in() - start).expect("consumed input fits in usize");
            if taken == plain.len() && produced < chunk.len() {
                break;
            }
            if produced == 0 && taken == consumed {
                return Err(failed("compression", "no progress"));
            }
        }
        while !compressed.len().is_multiple_of(BASE64_GROUP) {
            compressed.extend_from_slice(&EMPTY_STORED_BLOCK);
        }
        compressed.extend_from_slice(&FINAL_EMPTY_BLOCK);
        Ok(encode(tag, &compressed))
    }
}

pub(crate) fn extensible(body: &str) -> bool {
    body.starts_with(RECORDS)
        && body.len() > 1 + SEAL
        && (body.len() - 1 - SEAL).is_multiple_of(BASE64_CHARS)
}

pub(crate) fn unpack(text: &str, dictionary: &[u8]) -> Result<Vec<u8>, StoreError> {
    let compressed = STANDARD_NO_PAD
        .decode(text)
        .map_err(|error| failed("decoding", &error.to_string()))?;
    inflate(&compressed, dictionary)
}

pub(crate) fn linked(text: &str) -> Result<(u64, Vec<u8>), StoreError> {
    let mut compressed = STANDARD_NO_PAD
        .decode(text)
        .map_err(|error| failed("decoding", &error.to_string()))?;
    let mut back: u64 = 0;
    let mut shift = 0;
    for (index, &byte) in compressed.iter().enumerate() {
        let low = u64::from(byte & (VARINT_MORE - 1));
        if shift >= u64::BITS || (low << shift) >> shift != low {
            return Err(failed("decoding", "the link back overflows"));
        }
        back |= low << shift;
        shift += VARINT_BITS;
        if byte & VARINT_MORE == 0 {
            compressed.drain(..=index);
            return Ok((back, compressed));
        }
    }
    Err(failed("decoding", "the row ends inside its link back"))
}

pub(crate) fn inflate(compressed: &[u8], dictionary: &[u8]) -> Result<Vec<u8>, StoreError> {
    let mut inflate =
        Inflate::new(-WINDOW_BITS).map_err(|error| failed("decompression", error.as_str()))?;
    if !dictionary.is_empty() {
        inflate
            .set_dictionary(dictionary)
            .map_err(|error| failed("decompression", error.as_str()))?;
    }
    let start = inflate.total_in();
    let mut plain = Vec::new();
    let mut chunk = vec![0; WINDOW];
    loop {
        let consumed =
            usize::try_from(inflate.total_in() - start).expect("consumed input fits in usize");
        let before = inflate.total_out();
        let status = inflate
            .inflate(&compressed[consumed..], &mut chunk, InflateFlush::NoFlush)
            .map_err(|error| failed("decompression", error.as_str()))?;
        let produced = usize::try_from(inflate.total_out() - before)
            .expect("one call produces at most its chunk");
        plain.extend_from_slice(&chunk[..produced]);
        match status {
            InflateStatus::StreamEnd => return Ok(plain),
            InflateStatus::Ok if produced > 0 || inflate.total_in() - start > consumed as u64 => {}
            InflateStatus::Ok | InflateStatus::BufError => {
                return Err(failed("decompression", "the row ends before its stream"));
            }
        }
    }
}

pub(crate) fn parts(body: &str, limit: Option<usize>) -> Result<Vec<&str>, StoreError> {
    let Some(limit) = limit else {
        return Ok(vec![body]);
    };
    if limit < 2 {
        return Err(StoreError::NotCommitted(format!(
            "a turn log row limit of {limit} bytes cannot hold a tag and a byte"
        )));
    }
    let (first, mut rest) = body.split_at(body.len().min(limit));
    let mut parts = vec![first];
    while !rest.is_empty() {
        let (part, next) = rest.split_at(rest.len().min(limit - 1));
        parts.push(part);
        rest = next;
    }
    Ok(parts)
}

const SEXTET_BITS: usize = 6;
const BYTE_BITS: usize = 8;
const PAD: char = '=';

const ALPHABET: &[u8; 1 << SEXTET_BITS] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
const NOT_BASE64: u8 = u8::MAX;
const SEXTETS: [u8; 1 << BYTE_BITS] = {
    let mut sextets = [NOT_BASE64; 1 << BYTE_BITS];
    let mut index = 0;
    while index < ALPHABET.len() {
        sextets[ALPHABET[index] as usize] = index as u8;
        index += 1;
    }
    sextets
};

pub(crate) fn padded(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let remainder = bytes.len() % BASE64_CHARS;
    if bytes.is_empty() || remainder == 1 {
        return None;
    }
    let mut last = 0;
    for &byte in bytes {
        last = SEXTETS[usize::from(byte)];
        if last == NOT_BASE64 {
            return None;
        }
    }
    let spare = (remainder * SEXTET_BITS) % BYTE_BITS;
    if last & ((1 << spare) - 1) != 0 {
        return None;
    }
    let mut padded = String::with_capacity(bytes.len() + BASE64_CHARS);
    padded.push_str(text);
    while !padded.len().is_multiple_of(BASE64_CHARS) {
        padded.push(PAD);
    }
    Some(padded)
}

pub(crate) fn binary(body: &str) -> Option<(&str, String)> {
    let tag = body.chars().next().filter(char::is_ascii)?;
    let (tag, text) = body.split_at(tag.len_utf8());
    padded(text).map(|padded| (tag, padded))
}

pub(crate) fn decoded_bytes(chars: usize) -> usize {
    assert_ne!(
        chars % BASE64_CHARS,
        1,
        "a cut of whole base64 groups and a final partial group"
    );
    chars * SEXTET_BITS / BYTE_BITS
}
