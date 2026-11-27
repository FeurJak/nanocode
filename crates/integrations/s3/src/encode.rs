pub use nanocode_aws::{hex, uri_encode, utc};

const BASE64_ALPHABET: &[u8; 64] =
    b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
const BASE64_GROUP_BYTES: usize = 3;
const BASE64_GROUP_CHARS: usize = 4;
const SHA256_BASE64_CHARS: usize = 44;

pub fn base64(bytes: &[u8]) -> String {
    let mut text =
        String::with_capacity(bytes.len().div_ceil(BASE64_GROUP_BYTES) * BASE64_GROUP_CHARS);
    for group in bytes.chunks(BASE64_GROUP_BYTES) {
        let b0 = u32::from(group[0]);
        let b1 = group.get(1).copied().map_or(0, u32::from);
        let b2 = group.get(2).copied().map_or(0, u32::from);
        let word = (b0 << 16) | (b1 << 8) | b2;
        let digit = |shift: u32| char::from(BASE64_ALPHABET[((word >> shift) & 0x3f) as usize]);
        text.push(digit(18));
        text.push(digit(12));
        text.push(if group.len() > 1 { digit(6) } else { '=' });
        text.push(if group.len() > 2 { digit(0) } else { '=' });
    }
    assert_eq!(text.len() % BASE64_GROUP_CHARS, 0);
    text
}

fn base64_value(character: u8) -> Option<u32> {
    BASE64_ALPHABET
        .iter()
        .position(|&candidate| candidate == character)
        .map(|position| position as u32)
}

pub fn base64_decode_sha256(text: &str) -> Option<[u8; 32]> {
    let characters = text.as_bytes();
    if characters.len() != SHA256_BASE64_CHARS || characters[SHA256_BASE64_CHARS - 1] != b'=' {
        return None;
    }
    let mut decoded = [0u8; 32];
    let mut written = 0;
    for group in characters.chunks(BASE64_GROUP_CHARS) {
        let mut word = 0u32;
        let mut digits = 0;
        for &character in group {
            if character == b'=' {
                break;
            }
            word = (word << 6) | base64_value(character)?;
            digits += 1;
        }
        let bytes = digits * 6 / 8;
        word <<= 6 * (BASE64_GROUP_CHARS - digits);
        for index in 0..bytes {
            if written == decoded.len() {
                return None;
            }
            decoded[written] = (word >> (16 - 8 * index)) as u8;
            written += 1;
        }
    }
    if written != decoded.len() || base64(&decoded) != text {
        return None;
    }
    Some(decoded)
}
