//! Intl.Segmenter (ECMA-402 18) as node 22 (ICU 78, Unicode 17) has it: grapheme clusters,
//! words and sentences of UAX #29 over the properties scripts/intl-loc/segdata.js takes
//! from the UCD into segdata.rs, with ICU's word statuses for isWordLike.
//!
//! Gap: ICU segments Han, Hiragana, half-width Katakana and Line_Break=SA (Thai, Lao,
//! Khmer, Myanmar) runs into words with its dictionaries; here each ideograph is a word, a
//! Hiragana run is one word and an SA run is one word.

use super::segdata::{PROPS, RANGES};
use super::Spec;
use crate::interp::conv::Fault;
use alloc::string::String;
use alloc::vec::Vec;

/// Intl.Segmenter ( locales, options ): the resolved options.
pub(super) fn resolve(locale: &str, raw: &Spec) -> Result<Spec, Fault> {
    let g = raw.choose("granularity", &["grapheme", "word", "sentence"], "grapheme", "Intl.Segmenter")?;
    let mut out = Spec::default();
    out.put("locale", locale);
    out.put("granularity", g);
    Ok(out)
}

fn props(c: u32) -> u32 {
    let key = (c << 8) | 0xff;
    let i = RANGES.partition_point(|r| *r <= key);
    PROPS[(RANGES[i.saturating_sub(1)] & 0xff) as usize]
}

#[derive(Clone, Copy)]
struct Ch {
    /// UTF-16 offset.
    at: usize,
    p: u32,
}

impl Ch {
    fn gcb(&self) -> u32 {
        self.p & 0xf
    }
    fn ep(&self) -> bool {
        self.p >> 4 & 1 == 1
    }
    fn incb(&self) -> u32 {
        self.p >> 5 & 3
    }
    fn wb(&self) -> u32 {
        self.p >> 7 & 0x1f
    }
    fn sb(&self) -> u32 {
        self.p >> 12 & 0xf
    }
    fn class(&self) -> u32 {
        self.p >> 16 & 7
    }
}

fn chars(s: &[u16]) -> Vec<Ch> {
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        let u = s[i] as u32;
        let (c, n) = if (0xd800..0xdc00).contains(&u) && i + 1 < s.len() && (0xdc00..0xe000).contains(&(s[i + 1] as u32)) {
            (0x10000 + ((u - 0xd800) << 10) + (s[i + 1] as u32 - 0xdc00), 2)
        } else {
            (u, 1)
        };
        // A lone surrogate is a Control (GCB), Other elsewhere.
        let p = if (0xd800..0xe000).contains(&c) { 3 } else { props(c) };
        out.push(Ch { at: i, p });
        i += n;
    }
    out
}

// Grapheme_Cluster_Break values (segdata.js's GCB order).
const G_CR: u32 = 1;
const G_LF: u32 = 2;
const G_CONTROL: u32 = 3;
const G_EXTEND: u32 = 4;
const G_ZWJ: u32 = 5;
const G_RI: u32 = 6;
const G_PREPEND: u32 = 7;
const G_SPACING: u32 = 8;
const G_L: u32 = 9;
const G_V: u32 = 10;
const G_T: u32 = 11;
const G_LV: u32 = 12;
const G_LVT: u32 = 13;
const INCB_LINKER: u32 = 1;
const INCB_CONSONANT: u32 = 2;
const INCB_EXTEND: u32 = 3;

/// The grapheme cluster boundaries (indexes into cs, with 0 and cs.len()).
// One branch per UAX #29 rule, so several give the same answer.
#[allow(clippy::if_same_then_else)]
fn graphemes(cs: &[Ch]) -> Vec<usize> {
    let mut b = alloc::vec![0];
    let mut ri = 0usize; // regional indicators in a row before i
    // GB9c state: after a consonant then extends/linkers; whether a linker was seen.
    let mut conj: Option<bool> = None;
    // GB11 state: ExtPict Extend* (then ZWJ).
    let mut pict = false;
    for i in 1..cs.len() {
        let (a, x) = (cs[i - 1], cs[i]);
        let (ga, gx) = (a.gcb(), x.gcb());
        // Update the states with a.
        ri = if ga == G_RI { ri + 1 } else { 0 };
        conj = match (a.incb(), conj) {
            (INCB_CONSONANT, _) => Some(false),
            (INCB_LINKER, Some(_)) => Some(true),
            (INCB_EXTEND, Some(l)) => Some(l),
            _ => None,
        };
        pict = a.ep() || (pict && ga == G_EXTEND);
        let pict_zwj = ga == G_ZWJ && i >= 2 && {
            // ExtPict Extend* ZWJ
            let mut j = i - 1;
            let mut ok = false;
            while j > 0 {
                j -= 1;
                if cs[j].gcb() == G_EXTEND {
                    continue;
                }
                ok = cs[j].ep();
                break;
            }
            ok
        };
        let join = if ga == G_CR && gx == G_LF {
            true
        } else if matches!(ga, G_CONTROL | G_CR | G_LF) || matches!(gx, G_CONTROL | G_CR | G_LF) {
            false
        } else if ga == G_L && matches!(gx, G_L | G_V | G_LV | G_LVT) {
            true
        } else if matches!(ga, G_LV | G_V) && matches!(gx, G_V | G_T) {
            true
        } else if matches!(ga, G_LVT | G_T) && gx == G_T {
            true
        } else if matches!(gx, G_EXTEND | G_ZWJ) || gx == G_SPACING || ga == G_PREPEND {
            true
        } else if x.incb() == INCB_CONSONANT && conj == Some(true) && matches!(a.incb(), INCB_LINKER | INCB_EXTEND) {
            true
        } else if pict_zwj && x.ep() {
            true
        } else {
            ga == G_RI && gx == G_RI && ri % 2 == 1
        };
        let _ = pict;
        if !join {
            b.push(i);
        }
    }
    if cs.is_empty() {
        b.clear();
        b.push(0);
    } else {
        b.push(cs.len());
    }
    b
}

// Word_Break values (segdata.js's WB order).
const W_CR: u32 = 1;
const W_LF: u32 = 2;
const W_NEWLINE: u32 = 3;
const W_EXTEND: u32 = 4;
const W_ZWJ: u32 = 5;
const W_RI: u32 = 6;
const W_FORMAT: u32 = 7;
const W_KATAKANA: u32 = 8;
const W_HL: u32 = 9;
const W_ALETTER: u32 = 10;
const W_SQ: u32 = 11;
const W_DQ: u32 = 12;
const W_MIDNUMLET: u32 = 13;
const W_MIDLETTER: u32 = 14;
const W_MIDNUM: u32 = 15;
const W_NUMERIC: u32 = 16;
const W_EXTENDNUMLET: u32 = 17;
const W_WSEGSPACE: u32 = 18;

/// ICU's word class of a character: the Word_Break value, with ideographs and kana
/// (class 1, 2) and SA letters (class 3) as their own classes.
const W_IDEO: u32 = 32;
const W_KANA: u32 = 33;
const W_SA: u32 = 34;
const W_HANGUL: u32 = 35;

fn wclass(c: &Ch) -> u32 {
    match c.class() {
        1 if c.wb() == 0 || c.wb() == W_ALETTER => W_IDEO,
        2 if c.wb() == 0 => W_KANA,
        3 if matches!(c.wb(), 0 | W_ALETTER) => W_SA,
        4 => W_HANGUL,
        _ => c.wb(),
    }
}

fn ahletter(w: u32) -> bool {
    w == W_ALETTER || w == W_HL
}
fn midletq(w: u32) -> bool {
    w == W_MIDNUMLET || w == W_SQ
}
fn ignorable(w: u32) -> bool {
    matches!(w, W_EXTEND | W_FORMAT | W_ZWJ)
}

/// The word boundaries.
fn words(cs: &[Ch]) -> Vec<usize> {
    let w: Vec<u32> = cs.iter().map(wclass).collect();
    let n = cs.len();
    // The previous and next non-ignorable (WB4) positions.
    let prev = |i: usize| -> Option<usize> {
        let mut j = i;
        while j > 0 {
            j -= 1;
            if !ignorable(w[j]) || matches!(w[j], W_CR | W_LF | W_NEWLINE) {
                return Some(j);
            }
            if j == 0 {
                return Some(0);
            }
        }
        None
    };
    let next = |i: usize| -> Option<usize> {
        let mut j = i + 1;
        while j < n {
            if !ignorable(w[j]) {
                return Some(j);
            }
            j += 1;
        }
        None
    };
    let mut b = alloc::vec![0];
    let mut ri = 0usize;
    for i in 1..n {
        let (a0, x) = (w[i - 1], w[i]);
        if !ignorable(a0) {
            ri = if a0 == W_RI { ri + 1 } else { 0 };
        }
        let join = (|| {
            if a0 == W_CR && x == W_LF {
                return true;
            }
            if matches!(a0, W_CR | W_LF | W_NEWLINE) || matches!(x, W_CR | W_LF | W_NEWLINE) {
                return false;
            }
            if a0 == W_ZWJ && cs[i].ep() {
                return true;
            }
            if a0 == W_WSEGSPACE && x == W_WSEGSPACE {
                return true;
            }
            if ignorable(x) {
                return true;
            }
            // WB4: a is the last non-ignorable before i.
            let Some(ai) = prev(i) else { return false };
            let a = w[ai];
            if matches!(a, W_CR | W_LF | W_NEWLINE) && ai != i - 1 {
                return false;
            }
            let before = |k: usize| prev(k).map(|j| w[j]);
            let after = next(i).map(|j| w[j]);
            if ahletter(a) && ahletter(x) {
                return true;
            }
            if ahletter(a) && (x == W_MIDLETTER || midletq(x)) && after.is_some_and(ahletter) {
                return true;
            }
            if (a == W_MIDLETTER || midletq(a)) && ahletter(x) && before(ai).is_some_and(ahletter) {
                return true;
            }
            if a == W_HL && x == W_SQ {
                return true;
            }
            if a == W_HL && x == W_DQ && after == Some(W_HL) {
                return true;
            }
            if a == W_DQ && x == W_HL && before(ai) == Some(W_HL) {
                return true;
            }
            if a == W_NUMERIC && x == W_NUMERIC {
                return true;
            }
            if ahletter(a) && x == W_NUMERIC {
                return true;
            }
            if a == W_NUMERIC && ahletter(x) {
                return true;
            }
            if (a == W_MIDNUM || midletq(a)) && x == W_NUMERIC && before(ai) == Some(W_NUMERIC) {
                return true;
            }
            if a == W_NUMERIC && (x == W_MIDNUM || midletq(x)) && after == Some(W_NUMERIC) {
                return true;
            }
            if a == W_KATAKANA && x == W_KATAKANA {
                return true;
            }
            if (ahletter(a) || matches!(a, W_NUMERIC | W_KATAKANA | W_EXTENDNUMLET)) && x == W_EXTENDNUMLET {
                return true;
            }
            if a == W_EXTENDNUMLET && (ahletter(x) || matches!(x, W_NUMERIC | W_KATAKANA)) {
                return true;
            }
            // ICU: kana and SA runs stay together.
            // ICU: a Hangul syllable run ends at an Extend, Format or ZWJ.
            if a == W_HANGUL && x == W_HANGUL {
                return ai == i - 1;
            }
            if (a == W_KANA && x == W_KANA) || (a == W_SA && x == W_SA) {
                return true;
            }
            a == W_RI && x == W_RI && ri % 2 == 1
        })();
        if !join {
            b.push(i);
        }
    }
    if n == 0 {
        return alloc::vec![0];
    }
    b.push(n);
    b
}

/// ICU's isWordLike of a word segment: it holds a letter, number, kana or ideograph.
fn word_like(cs: &[Ch]) -> bool {
    // ICU's statuses: ZWJ ExtPict and a Hangul syllable with Extend, Format or ZWJ are none;
    // two or more ExtendNumLet alone are a word.
    if cs.windows(2).any(|w| w[0].wb() == W_ZWJ && w[1].ep()) {
        return false;
    }
    if cs.iter().any(|c| wclass(c) == W_HANGUL) && cs.iter().any(|c| ignorable(wclass(c))) {
        return false;
    }
    if cs.len() >= 2 && cs.iter().filter(|c| wclass(c) == W_EXTENDNUMLET).count() >= 2 {
        return true;
    }
    cs.iter().any(|c| {
        let w = wclass(c);
        ahletter(w) || matches!(w, W_NUMERIC | W_KATAKANA | W_IDEO | W_KANA | W_SA | W_HANGUL)
    })
}

// Sentence_Break values (segdata.js's SB order).
const S_CR: u32 = 1;
const S_LF: u32 = 2;
const S_EXTEND: u32 = 3;
const S_SEP: u32 = 4;
const S_FORMAT: u32 = 5;
const S_SP: u32 = 6;
const S_LOWER: u32 = 7;
const S_UPPER: u32 = 8;
const S_OLETTER: u32 = 9;
const S_NUMERIC: u32 = 10;
const S_ATERM: u32 = 11;
const S_SCONTINUE: u32 = 12;
const S_STERM: u32 = 13;
const S_CLOSE: u32 = 14;

fn parasep(s: u32) -> bool {
    matches!(s, S_SEP | S_CR | S_LF)
}
fn saterm(s: u32) -> bool {
    s == S_ATERM || s == S_STERM
}

/// The sentence boundaries.
fn sentences(cs: &[Ch]) -> Vec<usize> {
    let n = cs.len();
    if n == 0 {
        return alloc::vec![0];
    }
    // SB5: collapse Extend and Format onto the character before (except after ParaSep).
    let mut base: Vec<(usize, u32)> = Vec::new(); // (index of first char, class)
    for (i, c) in cs.iter().enumerate() {
        let s = c.sb();
        if matches!(s, S_EXTEND | S_FORMAT) && base.last().is_some_and(|l| !parasep(l.1)) {
            continue;
        }
        base.push((i, s));
    }
    let mut b = alloc::vec![0];
    let m = base.len();
    for k in 1..m {
        let (i, x) = base[k];
        let a = base[k - 1].1;
        let brk = (|| {
            if a == S_CR && x == S_LF {
                return false;
            }
            if parasep(a) {
                return true;
            }
            // Scan back over Sp* Close* to SATerm.
            let mut j = k;
            let mut sp = false;
            while j > 0 && base[j - 1].1 == S_SP {
                j -= 1;
                sp = true;
            }
            let mut close = false;
            while j > 0 && base[j - 1].1 == S_CLOSE {
                j -= 1;
                close = true;
            }
            let term = if j > 0 { Some(base[j - 1].1) } else { None };
            let Some(t) = term.filter(|t| saterm(*t)) else { return false };
            let tj = j - 1;
            if t == S_ATERM && !sp && !close && k == tj + 1 && x == S_NUMERIC {
                return false;
            }
            if t == S_ATERM && !sp && !close && k == tj + 1 && x == S_UPPER && tj > 0 && matches!(base[tj - 1].1, S_UPPER | S_LOWER) {
                return false;
            }
            if t == S_ATERM {
                // SB8: ( ¬(OLetter | Upper | Lower | ParaSep | SATerm) )* Lower
                let mut q = k;
                while q < m && !matches!(base[q].1, S_OLETTER | S_UPPER | S_LOWER) && !parasep(base[q].1) && !saterm(base[q].1) {
                    q += 1;
                }
                if q < m && base[q].1 == S_LOWER {
                    return false;
                }
            }
            if x == S_SCONTINUE || saterm(x) {
                return false;
            }
            if !sp && (x == S_CLOSE || x == S_SP || parasep(x)) {
                return false;
            }
            if x == S_SP || parasep(x) {
                return false;
            }
            true
        })();
        if brk {
            b.push(i);
        }
    }
    b.push(n);
    b
}

/// The segments of s by granularity, as V8's ICU break iterator gives them: their UTF-16
/// starts (and the end) joined by ",", then for words "|" and each segment's isWordLike
/// as 1 or 0.
pub(super) fn segment(spec: &Spec, s: &[u16]) -> String {
    let cs = chars(s);
    let g = spec.get("granularity").unwrap_or("grapheme");
    let b = match g {
        "word" => words(&cs),
        "sentence" => sentences(&cs),
        _ => graphemes(&cs),
    };
    let at = |i: usize| if i < cs.len() { cs[i].at } else { s.len() };
    let mut out = String::new();
    for (k, i) in b.iter().enumerate() {
        if k > 0 {
            out.push(',');
        }
        out.push_str(&alloc::format!("{}", at(*i)));
    }
    if g == "word" {
        out.push('|');
        for w in b.windows(2) {
            out.push(if word_like(&cs[w[0]..w[1]]) { '1' } else { '0' });
        }
    }
    out
}
