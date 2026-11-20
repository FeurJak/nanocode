mod collate;
mod data;
mod date;
mod number;
mod numdata;
mod text;

use crate::heap::{Heap, ObjectClass};
use crate::interp::conv::{Fault, type_error};
use crate::value::Value;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;



pub(crate) fn range<T>(msg: &str) -> Result<T, Fault> {
    crate::builtins::typed::range_error(msg)
}

pub(crate) fn type_err<T>(msg: &str) -> Result<T, Fault> {
    type_error(msg)
}

pub(crate) fn parse_f64(s: &str) -> f64 {
    match number::parse_decimal(s) {
        number::Num::Dec(d) => {
            let mut v = 0.0;
            for (i, x) in d.digits.iter().enumerate() {
                v += f64::from(*x) * pow10(d.exp - 1 - i as i32);
            }
            if d.neg { -v } else { v }
        }
        number::Num::Inf(n) => if n { f64::NEG_INFINITY } else { f64::INFINITY },
        number::Num::Nan => f64::NAN,
    }
}

fn pow10(e: i32) -> f64 {
    let mut v = 1.0;
    for _ in 0..e.unsigned_abs() {
        v *= 10.0;
    }
    if e < 0 { 1.0 / v } else { v }
}

#[derive(Default)]
pub(crate) struct Spec {
    pairs: Vec<(String, String)>,
    later: Vec<(String, String)>,
}

impl Spec {
    pub fn parse(s: &str) -> Spec {
        let mut spec = Spec::default();
        for pair in s.split('\u{1}') {
            if let Some((k, v)) = pair.split_once('\u{2}') {
                spec.pairs.push((k.to_string(), v.to_string()));
            }
        }
        spec
    }
    pub fn get(&self, k: &str) -> Option<&str> {
        self.pairs.iter().find(|(n, _)| n == k).map(|(_, v)| v.as_str())
    }
    pub fn int(&self, k: &str, default: u32) -> u32 {
        self.get(k)
            .and_then(|v| v.trim_start_matches('#').parse::<u32>().ok())
            .unwrap_or(default)
    }
    pub fn put(&mut self, k: &str, v: &str) {
        self.pairs.push((k.to_string(), v.to_string()));
    }
    pub fn num(&mut self, k: &str, v: u32) {
        self.put(k, &format!("#{v}"));
    }
    pub fn later(&mut self, k: &str, v: &str) {
        self.later.push((k.to_string(), v.to_string()));
    }
    pub fn flush_later(&mut self) {
        let later = core::mem::take(&mut self.later);
        self.pairs.extend(later);
    }
    pub fn choose(&self, k: &str, values: &[&'static str], default: &'static str, ctor: &str) -> Result<&'static str, Fault> {
        match self.get(k) {
            None => Ok(default),
            Some(v) => match values.iter().find(|x| **x == v) {
                Some(x) => Ok(x),
                None => range(&format!("Value {v} out of range for {ctor} options property {k}")),
            },
        }
    }
    pub fn choose_opt(&self, k: &str, values: &[&'static str], ctor: &str) -> Result<Option<&'static str>, Fault> {
        match self.get(k) {
            None => Ok(None),
            Some(_) => self.choose(k, values, "", ctor).map(Some),
        }
    }
    pub fn encode(&self) -> String {
        let mut s = String::new();
        for (i, (k, v)) in self.pairs.iter().enumerate() {
            if i > 0 {
                s.push('\u{1}');
            }
            s.push_str(k);
            s.push('\u{2}');
            s.push_str(v);
        }
        s
    }
}

#[derive(Default)]
pub(crate) struct Parts {
    pub items: Vec<(&'static str, String)>,
}

impl Parts {
    pub fn push(&mut self, ty: &'static str, v: &str) {
        if v.is_empty() {
            return;
        }
        if ty == "literal"
            && let Some((t, last)) = self.items.last_mut()
            && *t == "literal"
        {
            last.push_str(v);
            return;
        }
        self.items.push((ty, v.to_string()));
    }
    pub fn extend(&mut self, other: &Parts) {
        for (t, v) in &other.items {
            self.push(t, v);
        }
    }
    pub fn text(&self) -> String {
        let mut s = String::new();
        for (_, v) in &self.items {
            s.push_str(v);
        }
        s
    }
    pub fn encode(&self) -> String {
        let mut s = String::new();
        for (i, (t, v)) in self.items.iter().enumerate() {
            if i > 0 {
                s.push('\u{1}');
            }
            s.push_str(t);
            s.push('\u{2}');
            s.push_str(v);
        }
        s
    }
}

pub(crate) fn plural_en_like(lang: &str, i: u64, v: u32, f: u64, ordinal: bool) -> &'static str {
    if ordinal {
        if lang != "en" || v != 0 {
            return if lang == "fr" && v == 0 && i == 1 { "one" } else { "other" };
        }
        return match (i % 10, i % 100) {
            (1, m) if m != 11 => "one",
            (2, m) if m != 12 => "two",
            (3, m) if m != 13 => "few",
            _ => "other",
        };
    }
    match lang {
        "ja" | "zh" | "ko" => "other",
        "fr" | "pt" => if i <= 1 { "one" } else if v == 0 && i != 0 && i.is_multiple_of(1_000_000) { "many" } else { "other" },
        "es" | "it" => if i == 1 && (v == 0 || (lang == "es" && f == 0)) { "one" } else if v == 0 && i != 0 && i.is_multiple_of(1_000_000) { "many" } else { "other" },
        "ru" => {
            if v != 0 {
                return "other";
            }
            match (i % 10, i % 100) {
                (1, m) if m != 11 => "one",
                (2..=4, m) if !(12..=14).contains(&m) => "few",
                _ => "many",
            }
        }
        _ => if i == 1 && v == 0 { "one" } else { "other" },
    }
}

/// The categories PluralRules' resolvedOptions lists, in ICU's order (sorted, other last).
pub(crate) fn plural_categories(lang: &str, ordinal: bool) -> &'static str {
    if ordinal {
        return match lang {
            "en" => "few,one,two,other",
            "fr" => "one,other",
            _ => "other",
        };
    }
    match lang {
        "ja" | "zh" | "ko" => "other",
        "fr" | "pt" | "es" | "it" => "many,one,other",
        "ru" => "few,many,one,other",
        _ => "one,other",
    }
}

pub(crate) const LANGS: [&str; 11] = ["en", "de", "fr", "es", "it", "pt", "nl", "ja", "zh", "ko", "ru"];

fn canonical_tag(tag: &str) -> Option<String> {
    let subtags: Vec<&str> = tag.split('-').collect();
    let lang = subtags.first()?;
    let alpha = |s: &str| s.bytes().all(|b| b.is_ascii_alphabetic());
    if !alpha(lang) || !(lang.len() == 2 || lang.len() == 3 || (5..=8).contains(&lang.len())) {
        return None;
    }
    let mut out = lang.to_ascii_lowercase();
    let mut ext = false;
    for (i, s) in subtags.iter().enumerate().skip(1) {
        if s.is_empty() || s.len() > 8 || !s.bytes().all(|b| b.is_ascii_alphanumeric()) {
            return None;
        }
        out.push('-');
        if s.len() == 1 {
            ext = true;
        }
        if !ext && i == 1 && s.len() == 4 && alpha(s) {
            out.push_str(&s[..1].to_ascii_uppercase());
            out.push_str(&s[1..].to_ascii_lowercase());
        } else if !ext && s.len() == 2 && alpha(s) {
            out.push_str(&s.to_ascii_uppercase());
        } else {
            out.push_str(&s.to_ascii_lowercase());
        }
    }
    if subtags.last().is_some_and(|s| s.len() == 1) {
        return None;
    }
    Some(out)
}

fn canonical_list(tags: &str) -> Result<Vec<String>, Fault> {
    let mut out: Vec<String> = Vec::new();
    if tags.is_empty() {
        return Ok(out);
    }
    for t in tags.split('\n') {
        let Some(c) = canonical_tag(t) else {
            return range("Incorrect locale information provided");
        };
        if !out.contains(&c) {
            out.push(c);
        }
    }
    Ok(out)
}

pub(crate) fn resolve_locale(tags: &[String]) -> String {
    for t in tags {
        let base: Vec<&str> = t.split('-').take_while(|s| s.len() != 1).collect();
        if LANGS.contains(&base[0]) {
            let mut keep: Vec<&str> = alloc::vec![base[0]];
            for s in &base[1..] {
                if (s.len() == 4 && keep.len() == 1) || ((s.len() == 2 || s.len() == 3) && s.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())) {
                    keep.push(s);
                }
            }
            return keep.join("-");
        }
    }
    "en-US".to_string()
}

fn str_arg(heap: &Heap, v: Value) -> Option<String> {
    v.as_string().map(|s| heap.str_to_string(s))
}

fn string_value(heap: &mut Heap, s: &str) -> Result<Value, Fault> {
    Ok(Value::string(heap.alloc_str(s)?))
}

pub(crate) fn intrinsic(heap: &mut Heap, k: u16, a: [Value; 5]) -> Result<Option<Value>, Fault> {
    Ok(Some(match k {
        0 => {
            let (Some(proto), Some(kind), Some(spec)) = (a[0].as_object(), a[1].as_int(), a[2].as_string()) else {
                return Ok(None);
            };
            let o = heap.alloc_object(ObjectClass::Intl, Some(proto), 3)?;
            heap.set_object_slot(o, 0, Value::int(kind));
            heap.set_object_slot(o, 1, Value::string(spec));
            heap.set_object_slot(o, 2, Value::UNDEFINED);
            Value::object(o)
        }
        1 => match a[0].as_object() {
            Some(o) if heap.object_class(o) == ObjectClass::Intl && heap.object_slot(o, 0) == a[1] => heap.object_slot(o, 1),
            _ => Value::UNDEFINED,
        },
        2 => {
            let Some(o) = a[0].as_object() else { return Ok(None) };
            if a[1] != Value::UNDEFINED {
                heap.set_object_slot(o, 2, a[1]);
            }
            heap.object_slot(o, 2)
        }
        3 => {
            let (Some(kind), Some(raw)) = (a[0].as_int(), str_arg(heap, a[2])) else {
                return Ok(None);
            };
            let tags = canonical_list(&str_arg(heap, a[1]).unwrap_or_default())?;
            let locale = resolve_locale(&tags);
            let raw = Spec::parse(&raw);
            let spec = match kind {
                0 => number::resolve(&locale, &raw)?,
                1 => date::resolve(&locale, &raw)?,
                2 => collate::resolve(&locale, &tags, &raw)?,
                _ => text::resolve(kind, &locale, &raw)?,
            };
            string_value(heap, &spec.encode())?
        }
        4 => {
            let (Some(kind), Some(spec)) = (a[0].as_int(), str_arg(heap, a[1])) else {
                return Ok(None);
            };
            let spec = Spec::parse(&spec);
            let parts = a[4] == Value::TRUE;
            let out = match kind {
                0 => {
                    let num = |v: Value| match (v.as_string(), v.as_number()) {
                        (Some(s), _) => Some(number::parse_decimal(&heap.str_to_string(s))),
                        (None, Some(n)) => Some(number::from_f64(n)),
                        _ => None,
                    };
                    let Some(x) = num(a[2]) else { return Ok(None) };
                    if a[3] != Value::UNDEFINED {
                        // formatRange: parts are type \u{3} source.
                        let Some(y) = num(a[3]) else { return Ok(None) };
                        for (n, which) in [(&x, "start"), (&y, "end")] {
                            if matches!(n, number::Num::Nan) {
                                return range(&format!("Invalid {which}Range : NaN"));
                            }
                        }
                        let items = number::format_range(&spec, x, y);
                        let mut s = String::new();
                        for (i, (t, v, src)) in items.iter().enumerate() {
                            if parts {
                                if i > 0 {
                                    s.push('\u{1}');
                                }
                                s.push_str(t);
                                s.push('\u{3}');
                                s.push_str(src);
                                s.push('\u{2}');
                            }
                            s.push_str(v);
                        }
                        return Ok(Some(string_value(heap, &s)?));
                    }
                    number::format(&spec, x).parts
                }
                1 => {
                    let Some(t) = a[2].as_number() else { return Ok(None) };
                    date::format(&spec, t)?
                }
                2 => {
                    let (Some(x), Some(y)) = (a[2].as_string(), a[3].as_string()) else { return Ok(None) };
                    let (x, y): (Vec<u16>, Vec<u16>) = (heap.str_units(x).collect(), heap.str_units(y).collect());
                    return Ok(Some(Value::int(collate::compare(&spec, &x, &y))));
                }
                _ => {
                    let x = str_arg(heap, a[2]);
                    text::format(kind, &spec, a[2].as_number(), x.as_deref(), str_arg(heap, a[3]).as_deref())?
                }
            };
            let mut s = if parts { out.encode() } else { out.text() };
            // As V8 does: a date's text has plain spaces where its parts have U+202F.
            if kind == 1 && !parts {
                s = s.replace('\u{202f}', " ");
            }
            string_value(heap, &s)?
        }
        5 => {
            let tags = canonical_list(&str_arg(heap, a[0]).unwrap_or_default())?;
            let mut s = String::new();
            for (i, t) in tags.iter().enumerate() {
                if i > 0 {
                    s.push('\n');
                }
                s.push_str(t);
            }
            if a[1] == Value::TRUE {
                let supported: Vec<&String> = tags.iter().filter(|t| LANGS.contains(&t.split('-').next().unwrap_or(""))).collect();
                s = supported.iter().map(|t| t.as_str()).collect::<Vec<_>>().join("\n");
            }
            string_value(heap, &s)?
        }
        _ => return Ok(None),
    }))
}

pub(crate) fn locale_compare(a: &[u16], b: &[u16]) -> i32 {
    collate::compare(&Spec::default(), a, b)
}
