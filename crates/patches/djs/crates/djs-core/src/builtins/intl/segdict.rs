//! ICU 78's dictionary break engines (dictbe.cpp) for Intl.Segmenter's words: the
//! CjkBreakEngine for Han, Hiragana and Katakana over the word list of ICU 78.3's cjdict,
//! which scripts/intl-seg/gendict.py front codes into segdict/cj.dict.
//!
//! Each engine gets one run of the characters its set holds, as ICU's
//! DictionaryBreakEngine::findBreaks hands it, pushes the UTF-16 offsets of the breaks it
//! finds inside the run and returns ICU's count of them.

use crate::unicode::normalize::{self, Compositions, Form};
use alloc::vec::Vec;

static CJ: &[u8] = include_bytes!("segdict/cj.dict");

/// One front-coded word list (see gendict.py for the format).
struct Dict {
    data: &'static [u8],
    blocks: usize,
    values: bool,
    offsets_at: usize,
    words_at: usize,
    /// ICU's BytesDictionaryMatcher offset transform, for the SA dictionaries.
    transform: Option<u32>,
}

fn u16_at(d: &[u8], i: usize) -> u32 {
    u32::from(d[i]) | u32::from(d[i + 1]) << 8
}
fn u32_at(d: &[u8], i: usize) -> u32 {
    u16_at(d, i) | u16_at(d, i + 2) << 16
}

impl Dict {
    fn new(data: &'static [u8], transform: Option<u32>) -> Dict {
        let blocks = u32_at(data, 4) as usize;
        let ranks = u16_at(data, 8) as usize;
        let offsets_at = 12 + 2 * ranks;
        Dict {
            data,
            blocks,
            values: data[10] != 0,
            offsets_at,
            words_at: offsets_at + 4 * blocks,
            transform,
        }
    }

    /// Decodes the word at byte i onto cur (keeping its shared prefix): the next word's
    /// byte offset and this word's value.
    fn decode(&self, mut i: usize, cur: &mut Vec<u32>) -> (usize, u8) {
        let d = self.data;
        let h = d[i];
        i += 1;
        let (k, n) = if h >= 0xF0 {
            i += 2;
            (d[i - 2] as usize, d[i - 1] as usize)
        } else {
            ((h >> 4) as usize, (h & 0xF) as usize + 1)
        };
        cur.truncate(k);
        for _ in 0..n {
            let b = d[i] as usize;
            i += 1;
            let r = if b < 0x80 {
                b
            } else {
                i += 1;
                (b & 0x7F) << 8 | d[i - 1] as usize
            };
            cur.push(u16_at(d, 12 + 2 * r));
        }
        let mut v = 0;
        if self.values {
            v = d[i];
            i += 1;
        }
        (i, v)
    }

    fn block_start(&self, b: usize) -> usize {
        self.words_at + u32_at(self.data, self.offsets_at + 4 * b) as usize
    }

    /// The trie step for key: its value when it is a word, and whether a longer word
    /// starts with it (what ICU's UCharsTrie or BytesTrie answers for it).
    fn probe(&self, key: &[u32], w: &mut Vec<u32>) -> (Option<u8>, bool) {
        let (mut lo, mut hi) = (0usize, self.blocks);
        while hi - lo > 1 {
            let mid = (lo + hi) / 2;
            w.clear();
            self.decode(self.block_start(mid), w);
            if w.as_slice() <= key {
                lo = mid;
            } else {
                hi = mid;
            }
        }
        let end = self.data.len();
        let mut i = self.block_start(lo);
        w.clear();
        while i < end {
            let (next, v) = self.decode(i, w);
            i = next;
            if w.as_slice() >= key {
                let mut exact = None;
                if w.as_slice() == key {
                    exact = Some(v);
                    if i >= end {
                        return (exact, false);
                    }
                    self.decode(i, w);
                }
                return (exact, w.len() > key.len() && w.starts_with(key));
            }
        }
        (None, false)
    }

    /// BytesDictionaryMatcher::transform: a character outside the dictionary's 254 code
    /// points reads as U_SENTINEL, which the bytes trie takes as 0xFF, ZWJ's byte.
    fn map(&self, c: u32) -> u32 {
        match self.transform {
            Some(off) if c != 0x200C && c != 0x200D && !(off..=off + 0xFD).contains(&c) => 0x200D,
            _ => c,
        }
    }

    /// DictionaryMatcher::matches over text[start..]: the code unit and code point
    /// lengths and values of up to limit words, and the code points read.
    fn matches(&self, text: &[u32], start: usize, max_len: usize, limit: usize) -> Matches {
        let mut m = Matches::default();
        let (mut key, mut w) = (Vec::new(), Vec::new());
        let mut units = 0;
        for &c in &text[start..] {
            key.push(self.map(c));
            units += if c > 0xFFFF { 2 } else { 1 };
            m.prefix += 1;
            let (value, more) = self.probe(&key, &mut w);
            if let Some(v) = value
                && m.cu.len() < limit
            {
                m.cu.push(units);
                m.cp.push(key.len());
                m.values.push(u32::from(v));
            }
            if !more || units >= max_len {
                break;
            }
        }
        m
    }
}

#[derive(Default)]
struct Matches {
    cu: Vec<usize>,
    cp: Vec<usize>,
    values: Vec<u32>,
    prefix: usize,
}

/// A run of one engine's characters: code points, their UTF-16 offsets and the offset
/// after the run.
pub(super) struct Run<'a> {
    pub cps: &'a [u32],
    pub at: &'a [usize],
    pub end: usize,
}

/// The engine segdata numbers engine (1 CJ; 2 Thai, 3 Lao, 4 Myanmar and 5 Khmer find no
/// breaks yet) on run.
pub(super) fn find_breaks(engine: u32, run: &Run, breaks: &mut Vec<usize>) -> usize {
    match engine {
        1 => cjk(run, breaks),
        _ => 0,
    }
}

/// ICU's isKatakana.
fn is_katakana(c: u32) -> bool {
    ((0x30A1..=0x30FE).contains(&c) && c != 0x30FB) || (0xFF66..=0xFF9F).contains(&c)
}

fn katakana_cost(len: usize) -> u32 {
    const COST: [u32; 9] = [8192, 984, 408, 240, 204, 252, 300, 372, 480];
    COST.get(len).copied().unwrap_or(8192)
}

fn to_units(cps: &[u32]) -> Vec<u16> {
    let mut out = Vec::with_capacity(cps.len());
    for &c in cps {
        if c > 0xFFFF {
            let v = c - 0x10000;
            out.push((0xD800 + (v >> 10)) as u16);
            out.push((0xDC00 + (v & 0x3FF)) as u16);
        } else {
            out.push(c as u16);
        }
    }
    out
}

/// CjkBreakEngine::divideUpDictionaryRange for Chinese and Japanese (no phrase breaking):
/// the cheapest split of the NFKC form into dictionary words (cost: the dictionary's
/// value, 255 for a character it lacks, katakana runs by length).
fn cjk(run: &Run, breaks: &mut Vec<usize>) -> usize {
    let n0 = run.cps.len();
    if n0 == 0 {
        return 0;
    }
    let (range_start, range_end) = (run.at[0], run.end);
    let mut comp = Compositions::default();
    let units = to_units(run.cps);
    let mut text = Vec::new();
    let mut map = Vec::new();
    if normalize::normalize(&units, Form::Nfkc, &mut comp) == units {
        text.extend_from_slice(run.cps);
        map.extend_from_slice(run.at);
    } else {
        // Normalize fragment by fragment, each normalized character mapping to the start
        // of its fragment.
        let mut i = 0;
        while i < n0 {
            let s = i;
            i += 1;
            while i < n0 && !normalize::nfkc_has_boundary_before(run.cps[i]) {
                i += 1;
            }
            let frag = normalize::normalize(&to_units(&run.cps[s..i]), Form::Nfkc, &mut comp);
            for c in char::decode_utf16(frag.iter().copied()) {
                text.push(c.map_or(0xFFFD, |c| c as u32));
                map.push(run.at[s]);
            }
        }
    }
    map.push(range_end);
    let dict = Dict::new(CJ, None);
    let n = text.len();
    let mut best = alloc::vec![u32::MAX; n + 1];
    best[0] = 0;
    let mut prev = alloc::vec![0usize; n + 1];
    let mut is_prev_katakana = false;
    for i in 0..n {
        if best[i] == u32::MAX {
            continue;
        }
        let m = dict.matches(&text, i, 20, n);
        let (mut lengths, mut values) = (m.cp, m.values);
        if lengths.first() != Some(&1) {
            lengths.push(1);
            values.push(255);
        }
        for (&l, &v) in lengths.iter().zip(values.iter()) {
            let s = best[i] + v;
            if s < best[i + l] {
                best[i + l] = s;
                prev[i + l] = i;
            }
        }
        let kata = is_katakana(text[i]);
        if !is_prev_katakana && kata {
            let mut run_len = 1;
            while i + run_len < n && run_len < 20 && is_katakana(text[i + run_len]) {
                run_len += 1;
            }
            if run_len < 20 {
                let s = best[i] + katakana_cost(run_len);
                if s < best[i + run_len] {
                    best[i + run_len] = s;
                    prev[i + run_len] = i;
                }
            }
        }
        is_prev_katakana = kata;
    }
    let mut t: Vec<usize> = Vec::new();
    if best[n] == u32::MAX {
        t.push(n);
    } else {
        let mut i = n;
        while i > 0 {
            t.push(i);
            i = prev[i];
        }
    }
    if breaks.last().is_none_or(|&b| b < range_start) {
        t.push(0);
    }
    let mut count = 0usize;
    let mut last: Option<usize> = None;
    for &cp in t.iter().rev() {
        let p = map[cp];
        if last.is_none_or(|q| p > q) && p != range_start {
            breaks.push(p);
            count += 1;
        }
        last = Some(p);
    }
    if count > 0 && breaks.last() == Some(&range_end) {
        breaks.pop();
        count -= 1;
    }
    count
}
