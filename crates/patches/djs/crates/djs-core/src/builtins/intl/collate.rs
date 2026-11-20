//! Intl.Collator and String.prototype.localeCompare: the Unicode Collation Algorithm with
//! the CLDR root collation (ducet.rs), as ICU's root collator computes it: strings in NFD,
//! contractions (with discontiguous matching), prefix mappings, implicit weights for Han and
//! unassigned code points, variable weighting (non-ignorable, or shifted with
//! ignorePunctuation), numeric collation and caseFirst.
#[path = "ducet.rs"]
mod ducet;

use ducet::{
    COMMON_S, COMMON_T, CONTR, DIGIT_ZERO, DIGIT_ZEROS, IMPLICIT, MAP, POOL, PREFIX, SEQ,
    TRAILING, UNIFIED, VARIABLE_TOP,
};
use super::{Spec, range};
use crate::interp::conv::Fault;
use alloc::string::String;
use alloc::vec::Vec;
use core::cmp::Ordering;

/// One collation element. `p` is the primary as a sort key: the main weight in the high
/// half (0: ignorable), a sub-weight for implicit and numeric primaries in the low half.
#[derive(Clone, Copy, Debug)]
struct Ce {
    p: u64,
    s: u16,
    t: u8,
    /// 0 lower, 1 mixed, 2 upper.
    case: u8,
}

struct Options {
    numeric: bool,
    shifted: bool,
    case_first: u8,
    level: u8,
}

/// The sort-key position of a table primary rank: implicit primaries sit between the
/// explicit ones and the trailing ones (U+FFFD, U+FFFF).
fn main_key(rank: u32) -> u64 {
    if rank + 1 < TRAILING {
        u64::from(rank)
    } else if rank + 1 == TRAILING {
        // U+FDD1's primary: between Han and the other implicit primaries.
        0x4_0000
    } else {
        u64::from(rank) + 0x20_0000
    }
}

/// Han in radical-stroke order: runs of code points with consecutive positions, by start,
/// each `(start - 0x3400) | position << 18` in 5 little-endian bytes (gen-collation.py).
static HAN: &[u8] = include_bytes!("han.bin");

/// The radical-stroke position of a Han code point.
fn han_position(cp: u32) -> Option<u64> {
    if !UNIFIED.iter().any(|&(a, b)| (a..=b).contains(&cp)) {
        return None;
    }
    let run = |i: usize| {
        let b = &HAN[i * 5..i * 5 + 5];
        let v = b.iter().rev().fold(0u64, |v, x| v << 8 | u64::from(*x));
        ((v & 0x3_FFFF) as u32 + 0x3400, v >> 18)
    };
    let (mut lo, mut hi) = (0, HAN.len() / 5);
    while lo < hi {
        let mid = usize::midpoint(lo, hi);
        if run(mid).0 <= cp { lo = mid + 1 } else { hi = mid }
    }
    let (start, at) = run(lo.checked_sub(1)?);
    Some(at + u64::from(cp - start))
}

/// Implicit weights, after the table's explicit primaries: Han by radical and strokes, then
/// everything else by code point.
fn implicit(cp: u32, t: u8, case: u8) -> Ce {
    let key = match han_position(cp) {
        Some(at) => 0x1_0000 + at,
        None => 0x4_0001 + u64::from(cp),
    };
    Ce { p: key << 32, s: COMMON_S, t, case }
}

fn unpack(w: u32) -> Ce {
    let rank = w >> 16;
    Ce {
        p: if rank == 0 { 0 } else { main_key(rank) << 32 },
        s: ((w >> 7) & 0x1FF) as u16,
        t: ((w >> 2) & 31) as u8,
        case: (w & 3) as u8,
    }
}

fn push_words(words: &[u32], out: &mut Vec<Ce>) {
    let mut i = 0;
    while i < words.len() {
        if words[i] == IMPLICIT {
            let x = words[i + 1];
            out.push(implicit(x & 0x1F_FFFF, ((x >> 21) & 31) as u8, ((x >> 26) & 3) as u8));
            i += 2;
        } else {
            out.push(unpack(words[i]));
            i += 1;
        }
    }
}

fn pool(e: u32) -> &'static [u32] {
    let at = (e >> 5) as usize;
    &POOL[at..at + (e & 31) as usize]
}

/// The CEs of one code point without context.
fn single(cp: u32, out: &mut Vec<Ce>) {
    let k = MAP.partition_point(|e| e.0 >> 11 <= cp);
    if k > 0 {
        let (head, w) = MAP[k - 1];
        let start = head >> 11;
        if cp - start <= head & 0x1FF {
            match (head >> 9) & 3 {
                0 => out.push(unpack(w + ((cp - start) << 16))),
                1 => out.push(unpack(w)),
                _ => push_words(pool(w), out),
            }
            return;
        }
    }
    out.push(implicit(cp, COMMON_T, 0));
}

#[cfg(feature = "unicode-normalization")]
fn ccc(c: u32) -> u8 {
    crate::unicode::normalize::ccc(c)
}

#[cfg(not(feature = "unicode-normalization"))]
fn ccc(c: u32) -> u8 {
    u8::from((0x300..0x370).contains(&c))
}

/// The longest contraction starting at `i`: its table entry and the positions it takes.
fn contraction(cps: &[u32], used: &[bool], i: usize) -> Option<(u32, Vec<usize>)> {
    let c = cps[i];
    let first = |e: &(u32, u32)| SEQ[(e.0 >> 3) as usize];
    let from = CONTR.partition_point(|e| first(e) < c);
    'next: for e in CONTR[from..].iter().take_while(|e| first(e) == c) {
        let at = (e.0 >> 3) as usize;
        let seq = &SEQ[at + 1..at + (e.0 & 7) as usize];
        let mut taken = Vec::new();
        let mut j = i + 1;
        // The highest combining class skipped over (a discontiguous match).
        let mut skipped: Option<u8> = None;
        for &want in seq {
            loop {
                if j >= cps.len() {
                    continue 'next;
                }
                if used[j] {
                    j += 1;
                    continue;
                }
                let cc = ccc(cps[j]);
                if cps[j] == want && skipped.is_none_or(|s| cc != 0 && s < cc) {
                    taken.push(j);
                    j += 1;
                    break;
                }
                if cc == 0 {
                    continue 'next;
                }
                skipped = Some(skipped.map_or(cc, |s| s.max(cc)));
                j += 1;
            }
        }
        return Some((e.1, taken));
    }
    None
}

fn digit(c: u32) -> Option<u32> {
    let k = DIGIT_ZEROS.partition_point(|z| *z <= c);
    let z = *DIGIT_ZEROS.get(k.checked_sub(1)?)?;
    (c - z < 10).then_some(c - z)
}

fn code_points(units: &[u16]) -> Vec<u32> {
    #[cfg(feature = "unicode-normalization")]
    let nfd = crate::unicode::normalize::normalize(
        units,
        crate::unicode::normalize::Form::Nfd,
        &mut crate::unicode::normalize::Compositions::default(),
    );
    #[cfg(feature = "unicode-normalization")]
    let units = &nfd[..];
    // A lone surrogate collates as its own (unassigned) code point.
    char::decode_utf16(units.iter().copied())
        .map(|r| r.map_or_else(|e| u32::from(e.unpaired_surrogate()), u32::from))
        .collect()
}

fn elems(units: &[u16], o: &Options) -> Vec<Ce> {
    let cps = code_points(units);
    let mut out: Vec<Ce> = Vec::new();
    let mut used = alloc::vec![false; cps.len()];
    let zero = main_key(DIGIT_ZERO) << 32;
    let mut i = 0;
    while i < cps.len() {
        if used[i] {
            i += 1;
            continue;
        }
        let c = cps[i];
        if o.numeric && digit(c).is_some() {
            let mut run = Vec::new();
            while i < cps.len() && !used[i] {
                let Some(d) = digit(cps[i]) else { break };
                if !(run.is_empty() && d == 0) {
                    run.push(d);
                }
                i += 1;
            }
            if run.is_empty() {
                run.push(0);
            }
            out.push(Ce { p: zero | run.len() as u64, s: COMMON_S, t: COMMON_T, case: 0 });
            for d in run {
                out.push(Ce { p: zero | u64::from(d + 1), s: COMMON_S, t: COMMON_T, case: 0 });
            }
            continue;
        }
        if i > 0
            && let Some(&(_, _, w)) = PREFIX.iter().find(|e| e.0 == c && e.1 == cps[i - 1])
        {
            out.push(unpack(w));
        } else if let Some((e, taken)) = contraction(&cps, &used, i) {
            for j in taken {
                used[j] = true;
            }
            push_words(pool(e), &mut out);
        } else {
            single(c, &mut out);
        }
        i += 1;
    }
    if o.shifted {
        // Variable CEs (spaces and punctuation) are ignored, with the ignorables after them.
        let mut after_variable = false;
        out.retain(|e| {
            let main = e.p >> 32;
            if main != 0 && main <= u64::from(VARIABLE_TOP) {
                after_variable = true;
                return false;
            }
            if main != 0 {
                after_variable = false;
            }
            !after_variable
        });
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
        shifted: spec.get("ignorePunctuation") == Some("!t"),
        case_first: match spec.get("caseFirst") {
            Some("upper") => 2,
            Some("lower") => 1,
            _ => 0,
        },
        level,
    }
}

fn cmp_level<K: Ord>(x: &[Ce], y: &[Ce], key: impl Fn(&Ce) -> Option<K>) -> Ordering {
    x.iter().filter_map(&key).cmp(y.iter().filter_map(&key))
}

pub(super) fn compare(spec: &Spec, a: &[u16], b: &[u16]) -> i32 {
    let o = options(spec);
    let (x, y) = (elems(a, &o), elems(b, &o));
    let case = |e: &Ce| if o.case_first == 2 { 2 - e.case } else { e.case };
    let mut r = cmp_level(&x, &y, |e| (e.p != 0).then_some(e.p));
    if r == Ordering::Equal && (o.level == 2 || o.level == 4) {
        r = cmp_level(&x, &y, |e| (e.s != 0).then_some(e.s));
    }
    if r == Ordering::Equal && o.level == 3 {
        r = cmp_level(&x, &y, |e| (e.p != 0).then_some(case(e)));
    }
    if r == Ordering::Equal && o.level == 4 {
        r = cmp_level(&x, &y, |e| {
            (e.t != 0).then_some(if o.case_first == 0 { (0, e.t) } else { (case(e), e.t) })
        });
    }
    match r {
        Ordering::Less => -1,
        Ordering::Equal => 0,
        Ordering::Greater => 1,
    }
}

/// The Unicode extension keywords (`-u-kn-true`) of a language tag, with their values.
pub(super) fn unicode_keywords(tag: &str) -> Vec<(&str, String)> {
    let mut out: Vec<(&str, String)> = Vec::new();
    let subtags = tag.split('-').skip_while(|s| !s.eq_ignore_ascii_case("u")).skip(1);
    for s in subtags.take_while(|s| s.len() != 1) {
        if s.len() == 2 {
            out.push((s, String::new()));
        } else if let Some((_, v)) = out.last_mut() {
            if !v.is_empty() {
                v.push('-');
            }
            v.push_str(s);
        }
    }
    out
}

/// ResolveLocale for Intl.Collator: `tags` are the requested locales, `locale` the one
/// chosen from them.
pub(super) fn resolve(locale: &str, tags: &[String], raw: &Spec) -> Result<Spec, Fault> {
    let n = "Intl.Collator";
    let usage = raw.choose("usage", &["sort", "search"], "sort", n)?;
    let sensitivity = raw.choose("sensitivity", &["base", "accent", "case", "variant"], "variant", n)?;
    let ignore = raw.get("ignorePunctuation") == Some("true");
    let lang = locale.split('-').next();
    let keywords = tags.iter().find(|t| t.split('-').next() == lang).map(|t| unicode_keywords(t)).unwrap_or_default();
    let keyword = |k: &str| keywords.iter().find(|(key, _)| *key == k).map(|(_, v)| v.as_str());
    // The locale keeps a keyword whose value is supported and that no option overrides.
    let mut kept: Vec<String> = Vec::new();
    let kn = match keyword("kn") {
        Some("" | "true") => Some(true),
        Some("false") => Some(false),
        _ => None,
    };
    let numeric = match (raw.get("numeric"), kn) {
        (Some(o), Some(k)) if (o == "true") != k => o == "true",
        (Some(o), None) => o == "true",
        (_, Some(k)) => {
            kept.push(String::from(if k { "kn" } else { "kn-false" }));
            k
        }
        (None, None) => false,
    };
    let kf = keyword("kf").filter(|v| matches!(*v, "upper" | "lower" | "false"));
    let case_first = match (raw.get("caseFirst"), kf) {
        (Some(_), _) => {
            let o = raw.choose("caseFirst", &["upper", "lower", "false"], "false", n)?;
            if kf == Some(o) {
                kept.push(alloc::format!("kf-{o}"));
            }
            o
        }
        (None, Some(k)) => {
            kept.push(alloc::format!("kf-{k}"));
            k
        }
        (None, None) => "false",
    };
    kept.sort();
    let locale = if kept.is_empty() { String::from(locale) } else { alloc::format!("{locale}-u-{}", kept.join("-")) };
    let locale = locale.as_str();
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
