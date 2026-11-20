use super::{Spec, range};
use crate::interp::conv::Fault;
use alloc::vec::Vec;

const ASCII: &[u8] = b" _-,;:!?.'\"()[]{}@*/\\&#%\x60^+<=>|~$0123456789aAbBcCdDeEfFgGhHiIjJkKlLmMnNoOpPqQrRsStTuUvVwWxXyYzZ";
const SPAN: u64 = 0x11_0000;

fn rank(b: u8) -> u64 {
    ASCII.iter().position(|c| *c == b).map_or(0, |i| i as u64 + 1) * SPAN
}

struct Elem {
    p: u64,
    s: u8,
    t: u8,
}

struct Options {
    numeric: bool,
    punct: bool,
    upper: bool,
    level: u8,
    es: bool,
}

fn push_letter(out: &mut Vec<Elem>, b: u8, s: u8, upper: bool, o: &Options) {
    let lower = b.to_ascii_lowercase();
    let p = if o.es && lower == b'n' && s == 9 { rank(b'n') + 1 } else { rank(lower) };
    let s = if p == rank(b'n') + 1 { 0 } else { s };
    out.push(Elem { p, s, t: u8::from(upper) });
}

fn elems(units: &[u16], o: &Options) -> Vec<Elem> {
    let mut out: Vec<Elem> = Vec::new();
    let cps: Vec<u32> = char::decode_utf16(units.iter().copied())
        .map(|r| r.map_or(0xFFFD, u32::from))
        .collect();
    let mut i = 0;
    while i < cps.len() {
        let c = cps[i];
        i += 1;
        if (0x300..0x370).contains(&c) {
            if let Some(last) = out.last_mut() {
                let m = MARKS.iter().position(|m| u32::from(*m) == c).map_or(30, |x| x as u8 + 1);
                last.s = last.s.saturating_mul(32).saturating_add(m);
            }
            continue;
        }
        if c < 0x20 || (0x7F..0xA0).contains(&c) {
            continue;
        }
        if c < 0x80 {
            let b = c as u8;
            if b.is_ascii_digit() && o.numeric {
                let start = i - 1;
                while i < cps.len() && (0x30..0x3A).contains(&cps[i]) {
                    i += 1;
                }
                let run = &cps[start..i];
                let lead = run.iter().take_while(|d| **d == 0x30).count().min(run.len() - 1);
                let run = &run[lead..];
                out.push(Elem { p: rank(b'0') + run.len() as u64, s: 0, t: 0 });
                for d in run {
                    out.push(Elem { p: rank(*d as u8), s: 0, t: 0 });
                }
                continue;
            }
            if b.is_ascii_alphabetic() {
                push_letter(&mut out, b, 0, b.is_ascii_uppercase(), o);
                continue;
            }
            if o.punct && !b.is_ascii_digit() {
                continue;
            }
            out.push(Elem { p: rank(b), s: 0, t: 0 });
            continue;
        }
        if let Ok(k) = LATIN.binary_search_by_key(&(c as u16), |e| e.0)
            && c < 0x10000
        {
            let (_, base, mark) = LATIN[k];
            let upper = base.is_ascii_uppercase();
            push_letter(&mut out, base, mark, upper, o);
            continue;
        }
        let expand: Option<(&[u8], u8)> = match c {
            0xE6 => Some((b"ae", 2)),
            0xC6 => Some((b"ae", 3)),
            0x153 => Some((b"oe", 2)),
            0x152 => Some((b"oe", 3)),
            0xDF => Some((b"ss", 2)),
            _ => None,
        };
        if let Some((letters, t)) = expand {
            for b in letters {
                out.push(Elem { p: rank(*b), s: 0, t });
            }
            continue;
        }
        let ch = char::from_u32(c).unwrap_or('\u{FFFD}');
        if ch.is_whitespace() {
            if !o.punct {
                out.push(Elem { p: rank(b' '), s: 0, t: 1 });
            }
            continue;
        }
        if ch.is_alphabetic() {
            let lower = ch.to_lowercase().next().unwrap_or(ch);
            let base = if c == 0xFE || c == 0xDE { u32::from(b'z') + 1 } else { u32::from(lower) };
            out.push(Elem { p: (rank(b'Z') + SPAN) + u64::from(base), s: 0, t: u8::from(ch.is_uppercase()) });
            continue;
        }
        if ch.is_numeric() {
            out.push(Elem { p: rank(b'9') + 1 + u64::from(c), s: 0, t: 0 });
            continue;
        }
        if !o.punct {
            out.push(Elem { p: rank(b'$') + u64::from(c), s: 0, t: 0 });
        }
    }
    out
}

fn options(spec: &Spec) -> Options {
    let level = match spec.get("sensitivity") {
        Some("base") => 1,
        Some("accent") => 2,
        Some("case") => 3,
        _ => 4,
    };
    Options {
        numeric: spec.get("numeric") == Some("!t"),
        punct: spec.get("ignorePunctuation") == Some("!t"),
        upper: spec.get("caseFirst") == Some("upper"),
        level,
        es: spec.get("locale").is_some_and(|l| l.starts_with("es")),
    }
}

pub(super) fn compare(spec: &Spec, a: &[u16], b: &[u16]) -> i32 {
    let o = options(spec);
    let (x, y) = (elems(a, &o), elems(b, &o));
    let ord = |c: core::cmp::Ordering| match c {
        core::cmp::Ordering::Less => -1,
        core::cmp::Ordering::Equal => 0,
        core::cmp::Ordering::Greater => 1,
    };
    let r = ord(x.iter().map(|e| e.p).cmp(y.iter().map(|e| e.p)));
    if r != 0 || o.level == 1 {
        return r;
    }
    if o.level != 3 {
        let r = ord(x.iter().map(|e| e.s).cmp(y.iter().map(|e| e.s)));
        if r != 0 || o.level == 2 {
            return r;
        }
    }
    let t = |e: &Elem| if o.upper { 3 - e.t.min(3) } else { e.t };
    ord(x.iter().map(t).cmp(y.iter().map(t)))
}

pub(super) fn resolve(locale: &str, raw: &Spec) -> Result<Spec, Fault> {
    let n = "Intl.Collator";
    let usage = raw.choose("usage", &["sort", "search"], "sort", n)?;
    let sensitivity = raw.choose("sensitivity", &["base", "accent", "case", "variant"], "variant", n)?;
    let ignore = raw.get("ignorePunctuation") == Some("true");
    let numeric = raw.get("numeric") == Some("true");
    let case_first = raw.choose("caseFirst", &["upper", "lower", "false"], "false", n)?;
    if let Some(c) = raw.get("collation")
        && (c.len() < 3 || c.len() > 8 || !c.bytes().all(|b| b.is_ascii_alphanumeric()))
    {
        return range(&alloc::format!("Invalid collation : {c}"));
    }
    let mut out = Spec::default();
    out.put("locale", locale);
    out.put("usage", usage);
    out.put("sensitivity", sensitivity);
    out.put("ignorePunctuation", if ignore { "!t" } else { "!f" });
    out.put("collation", "default");
    out.put("numeric", if numeric { "!t" } else { "!f" });
    out.put("caseFirst", case_first);
    Ok(out)
}

const MARKS: [u16; 21] = [0x301, 0x300, 0x306, 0x302, 0x30C, 0x30A, 0x308, 0x30B, 0x303, 0x307, 0x338, 0x327, 0x328, 0x304, 0x309, 0x31B, 0x323, 0x326, 0x331, 0x335, 0x337];
const LATIN: [(u16, u8, u8); 227] = [
    (0xC0, b'A', 2),
    (0xC1, b'A', 1),
    (0xC2, b'A', 4),
    (0xC3, b'A', 9),
    (0xC4, b'A', 7),
    (0xC5, b'A', 6),
    (0xC7, b'C', 12),
    (0xC8, b'E', 2),
    (0xC9, b'E', 1),
    (0xCA, b'E', 4),
    (0xCB, b'E', 7),
    (0xCC, b'I', 2),
    (0xCD, b'I', 1),
    (0xCE, b'I', 4),
    (0xCF, b'I', 7),
    (0xD1, b'N', 9),
    (0xD2, b'O', 2),
    (0xD3, b'O', 1),
    (0xD4, b'O', 4),
    (0xD5, b'O', 9),
    (0xD6, b'O', 7),
    (0xD8, b'O', 11),
    (0xD9, b'U', 2),
    (0xDA, b'U', 1),
    (0xDB, b'U', 4),
    (0xDC, b'U', 7),
    (0xDD, b'Y', 1),
    (0xE0, b'a', 2),
    (0xE1, b'a', 1),
    (0xE2, b'a', 4),
    (0xE3, b'a', 9),
    (0xE4, b'a', 7),
    (0xE5, b'a', 6),
    (0xE7, b'c', 12),
    (0xE8, b'e', 2),
    (0xE9, b'e', 1),
    (0xEA, b'e', 4),
    (0xEB, b'e', 7),
    (0xEC, b'i', 2),
    (0xED, b'i', 1),
    (0xEE, b'i', 4),
    (0xEF, b'i', 7),
    (0xF1, b'n', 9),
    (0xF2, b'o', 2),
    (0xF3, b'o', 1),
    (0xF4, b'o', 4),
    (0xF5, b'o', 9),
    (0xF6, b'o', 7),
    (0xF8, b'o', 11),
    (0xF9, b'u', 2),
    (0xFA, b'u', 1),
    (0xFB, b'u', 4),
    (0xFC, b'u', 7),
    (0xFD, b'y', 1),
    (0xFF, b'y', 7),
    (0x100, b'A', 14),
    (0x101, b'a', 14),
    (0x102, b'A', 3),
    (0x103, b'a', 3),
    (0x104, b'A', 13),
    (0x105, b'a', 13),
    (0x106, b'C', 1),
    (0x107, b'c', 1),
    (0x108, b'C', 4),
    (0x109, b'c', 4),
    (0x10A, b'C', 10),
    (0x10B, b'c', 10),
    (0x10C, b'C', 5),
    (0x10D, b'c', 5),
    (0x10E, b'D', 5),
    (0x10F, b'd', 5),
    (0x110, b'D', 20),
    (0x111, b'd', 20),
    (0x112, b'E', 14),
    (0x113, b'e', 14),
    (0x114, b'E', 3),
    (0x115, b'e', 3),
    (0x116, b'E', 10),
    (0x117, b'e', 10),
    (0x118, b'E', 13),
    (0x119, b'e', 13),
    (0x11A, b'E', 5),
    (0x11B, b'e', 5),
    (0x11C, b'G', 4),
    (0x11D, b'g', 4),
    (0x11E, b'G', 3),
    (0x11F, b'g', 3),
    (0x120, b'G', 10),
    (0x121, b'g', 10),
    (0x122, b'G', 12),
    (0x123, b'g', 12),
    (0x124, b'H', 4),
    (0x125, b'h', 4),
    (0x128, b'I', 9),
    (0x129, b'i', 9),
    (0x12A, b'I', 14),
    (0x12B, b'i', 14),
    (0x12C, b'I', 3),
    (0x12D, b'i', 3),
    (0x12E, b'I', 13),
    (0x12F, b'i', 13),
    (0x130, b'I', 10),
    (0x131, b'i', 10),
    (0x134, b'J', 4),
    (0x135, b'j', 4),
    (0x136, b'K', 12),
    (0x137, b'k', 12),
    (0x139, b'L', 1),
    (0x13A, b'l', 1),
    (0x13B, b'L', 12),
    (0x13C, b'l', 12),
    (0x13D, b'L', 5),
    (0x13E, b'l', 5),
    (0x141, b'L', 21),
    (0x142, b'l', 21),
    (0x143, b'N', 1),
    (0x144, b'n', 1),
    (0x145, b'N', 12),
    (0x146, b'n', 12),
    (0x147, b'N', 5),
    (0x148, b'n', 5),
    (0x14C, b'O', 14),
    (0x14D, b'o', 14),
    (0x14E, b'O', 3),
    (0x14F, b'o', 3),
    (0x150, b'O', 8),
    (0x151, b'o', 8),
    (0x154, b'R', 1),
    (0x155, b'r', 1),
    (0x156, b'R', 12),
    (0x157, b'r', 12),
    (0x158, b'R', 5),
    (0x159, b'r', 5),
    (0x15A, b'S', 1),
    (0x15B, b's', 1),
    (0x15C, b'S', 4),
    (0x15D, b's', 4),
    (0x15E, b'S', 12),
    (0x15F, b's', 12),
    (0x160, b'S', 5),
    (0x161, b's', 5),
    (0x162, b'T', 12),
    (0x163, b't', 12),
    (0x164, b'T', 5),
    (0x165, b't', 5),
    (0x168, b'U', 9),
    (0x169, b'u', 9),
    (0x16A, b'U', 14),
    (0x16B, b'u', 14),
    (0x16C, b'U', 3),
    (0x16D, b'u', 3),
    (0x16E, b'U', 6),
    (0x16F, b'u', 6),
    (0x170, b'U', 8),
    (0x171, b'u', 8),
    (0x172, b'U', 13),
    (0x173, b'u', 13),
    (0x174, b'W', 4),
    (0x175, b'w', 4),
    (0x176, b'Y', 4),
    (0x177, b'y', 4),
    (0x178, b'Y', 7),
    (0x179, b'Z', 1),
    (0x17A, b'z', 1),
    (0x17B, b'Z', 10),
    (0x17C, b'z', 10),
    (0x17D, b'Z', 5),
    (0x17E, b'z', 5),
    (0x1A0, b'O', 16),
    (0x1A1, b'o', 16),
    (0x1AF, b'U', 16),
    (0x1B0, b'u', 16),
    (0x1CD, b'A', 5),
    (0x1CE, b'a', 5),
    (0x1CF, b'I', 5),
    (0x1D0, b'i', 5),
    (0x1D1, b'O', 5),
    (0x1D2, b'o', 5),
    (0x1D3, b'U', 5),
    (0x1D4, b'u', 5),
    (0x1D5, b'U', 7),
    (0x1D6, b'u', 7),
    (0x1D7, b'U', 7),
    (0x1D8, b'u', 7),
    (0x1D9, b'U', 7),
    (0x1DA, b'u', 7),
    (0x1DB, b'U', 7),
    (0x1DC, b'u', 7),
    (0x1DE, b'A', 7),
    (0x1DF, b'a', 7),
    (0x1E0, b'A', 10),
    (0x1E1, b'a', 10),
    (0x1E6, b'G', 5),
    (0x1E7, b'g', 5),
    (0x1E8, b'K', 5),
    (0x1E9, b'k', 5),
    (0x1EA, b'O', 13),
    (0x1EB, b'o', 13),
    (0x1EC, b'O', 13),
    (0x1ED, b'o', 13),
    (0x1F0, b'j', 5),
    (0x1F4, b'G', 1),
    (0x1F5, b'g', 1),
    (0x1F8, b'N', 2),
    (0x1F9, b'n', 2),
    (0x1FA, b'A', 6),
    (0x1FB, b'a', 6),
    (0x218, b'S', 18),
    (0x219, b's', 18),
    (0x21A, b'T', 18),
    (0x21B, b't', 18),
    (0x21E, b'H', 5),
    (0x21F, b'h', 5),
    (0x226, b'A', 10),
    (0x227, b'a', 10),
    (0x228, b'E', 12),
    (0x229, b'e', 12),
    (0x22A, b'O', 7),
    (0x22B, b'o', 7),
    (0x22C, b'O', 9),
    (0x22D, b'o', 9),
    (0x22E, b'O', 10),
    (0x22F, b'o', 10),
    (0x230, b'O', 10),
    (0x231, b'o', 10),
    (0x232, b'Y', 14),
    (0x233, b'y', 14),
];
