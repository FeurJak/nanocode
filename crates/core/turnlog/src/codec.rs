use nanocode_compression::{
    deflate::{Deflate, DeflateConfig, DeflateFlush, DeflateStatus, Strategy},
    inflate::{Inflate, InflateFlush, InflateStatus},
};
use nanocode_durability_core::StoreError;
use nanocode_sys::base64::{STANDARD, STANDARD_NO_PAD};

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
const BASE85_GROUP: usize = 4;
const BASE85_CHARS: usize = 5;
const BASE85_RADIX: u64 = 85;
const BASE85_ALPHABET: &[u8; BASE85_RADIX as usize] =
    b"0123456789abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ.-:+=^!/*?&<>()[]{}@%$#";
const NOT_BASE85: u8 = u8::MAX;
const BASE85_DIGITS: [u8; 1 << BYTE_BITS] = {
    let mut digits = [NOT_BASE85; 1 << BYTE_BITS];
    let mut index = 0;
    while index < BASE85_ALPHABET.len() {
        digits[BASE85_ALPHABET[index] as usize] = index as u8;
        index += 1;
    }
    digits
};
const BASE85_TOP_DIGIT: u64 = BASE85_RADIX - 1;
const _: () = assert!(FINAL_EMPTY_BLOCK.len() < BASE85_GROUP);
const _: () = assert!(SEAL == FINAL_EMPTY_BLOCK.len() + 1);
const _: () = assert!(EMPTY_STORED_BLOCK.len() % BASE85_GROUP == 1);
pub(crate) const SEGMENT: char = '~';

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
    let mut body = String::with_capacity(
        tag.len_utf8()
            + SEGMENT.len_utf8()
            + compressed.len().div_ceil(BASE85_GROUP) * BASE85_CHARS,
    );
    body.push(tag);
    body.push(SEGMENT);
    let mut chars = [0; BASE85_CHARS];
    for group in compressed.chunks(BASE85_GROUP) {
        let mut word = [0; BASE85_GROUP];
        word[..group.len()].copy_from_slice(group);
        let mut value = u64::from(u32::from_be_bytes(word));
        for char in chars.iter_mut().rev() {
            *char = BASE85_ALPHABET[usize::try_from(value % BASE85_RADIX).expect("a digit")];
            value /= BASE85_RADIX;
        }
        assert_eq!(value, 0, "four bytes fit five base85 digits");
        for &char in &chars[..=group.len()] {
            body.push(char::from(char));
        }
    }
    body
}

fn base85(segment: &str, bytes: &mut Vec<u8>) -> Result<(), StoreError> {
    for group in segment.as_bytes().chunks(BASE85_CHARS) {
        if group.len() == 1 {
            return Err(failed("decoding", "a base85 group holds one character"));
        }
        let mut value: u64 = 0;
        for index in 0..BASE85_CHARS {
            let digit = match group.get(index) {
                Some(&char) => match BASE85_DIGITS[usize::from(char)] {
                    NOT_BASE85 => return Err(failed("decoding", "a character is not base85")),
                    digit => u64::from(digit),
                },
                None => BASE85_TOP_DIGIT,
            };
            value = value * BASE85_RADIX + digit;
        }
        let word = u32::try_from(value)
            .map_err(|_| failed("decoding", "a base85 group overflows four bytes"))?;
        bytes.extend_from_slice(&word.to_be_bytes()[..group.len() - 1]);
    }
    Ok(())
}

fn decoded(text: &str) -> Result<Vec<u8>, StoreError> {
    let mut segments = text.split(SEGMENT);
    let first = segments.next().unwrap_or_default();
    let mut bytes = STANDARD_NO_PAD
        .decode(first)
        .map_err(|error| failed("decoding", &error.to_string()))?;
    for segment in segments {
        base85(segment, &mut bytes)?;
    }
    Ok(bytes)
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
        while !compressed.len().is_multiple_of(BASE85_GROUP) {
            compressed.extend_from_slice(&EMPTY_STORED_BLOCK);
        }
        compressed.extend_from_slice(&FINAL_EMPTY_BLOCK);
        Ok(encode(tag, &compressed))
    }
}

pub(crate) fn extensible(body: &str) -> bool {
    if !body.starts_with(RECORDS) {
        return false;
    }
    match body.rsplit_once(SEGMENT) {
        Some((_, last)) => last.len() > SEAL && (last.len() - SEAL).is_multiple_of(BASE85_CHARS),
        None => {
            body.len() > RECORDS.len_utf8() + SEAL
                && (body.len() - RECORDS.len_utf8() - SEAL).is_multiple_of(BASE64_CHARS)
        }
    }
}

pub(crate) fn unpack(text: &str, dictionary: &[u8]) -> Result<Vec<u8>, StoreError> {
    inflate(&decoded(text)?, dictionary)
}

pub(crate) fn linked(text: &str) -> Result<(u64, Vec<u8>), StoreError> {
    let mut compressed = decoded(text)?;
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

fn binary(body: &str, whole: bool) -> Option<(&str, String)> {
    let tag = body.chars().next().filter(char::is_ascii)?;
    let (tag, text) = body.split_at(tag.len_utf8());
    if text.contains(SEGMENT) {
        return whole.then(|| tail(text)).flatten().map(|padded| (tag, padded));
    }
    padded(text).map(|padded| (tag, padded))
}

pub(crate) fn binaries(rows: &[(u64, String)]) -> Vec<Option<(&str, String)>> {
    rows.iter()
        .enumerate()
        .map(|(index, (_, body))| {
            let whole = !body.starts_with(PART)
                && rows
                    .get(index + 1)
                    .is_none_or(|(_, next)| !next.starts_with(PART));
            binary(body, whole)
        })
        .collect()
}

pub(crate) fn tail(text: &str) -> Option<String> {
    if !text.contains(SEGMENT) {
        return padded(text);
    }
    let bytes = decoded(text).ok().filter(|bytes| !bytes.is_empty())?;
    Some(STANDARD.encode(bytes))
}

pub(crate) fn decoded_bytes(chars: usize) -> usize {
    assert_ne!(
        chars % BASE64_CHARS,
        1,
        "a cut of whole base64 groups and a final partial group"
    );
    chars * SEXTET_BITS / BYTE_BITS
}
