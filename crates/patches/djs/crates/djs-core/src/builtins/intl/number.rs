use super::data::{COMPACT, CURRENCIES, CURRENCY_SYMBOLS, NUM_LOCALES, UNIT_PER, UNIT_PER_PATTERNS, UNITS};
use super::{Parts, Spec, plural_en_like, range, type_err};
use crate::interp::conv::Fault;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

pub(crate) struct NumLocale {
    pub tag: &'static str,
    pub dec: &'static str,
    pub grp: &'static str,
    pub min2: bool,
    pub indian: bool,
    pub minus: &'static str,
    pub cur: &'static str,
    pub neg_cur: &'static str,
    pub acct: &'static str,
    pub pct: &'static str,
    pub neg_pct: &'static str,
    pub nan: &'static str,
}

pub(crate) struct Currency {
    pub code: &'static str,
    pub symbol: &'static str,
    pub narrow: &'static str,
    pub one: &'static str,
    pub other: &'static str,
    pub digits: u8,
}

pub(super) fn num_locale(tag: &str) -> &'static NumLocale {
    let lang = tag.split('-').next().unwrap_or("en");
    NUM_LOCALES
        .iter()
        .find(|l| l.tag == tag)
        .or_else(|| {
            let mut it = tag.split('-');
            let l = it.next().unwrap_or("");
            let r = it.find(|s| s.len() == 2 && s.bytes().all(|b| b.is_ascii_uppercase()));
            r.and_then(|r| NUM_LOCALES.iter().find(|n| n.tag.len() == 5 && n.tag.starts_with(l) && n.tag.ends_with(r)))
        })
        .or_else(|| NUM_LOCALES.iter().find(|l| l.tag == lang))
        .unwrap_or(&NUM_LOCALES[0])
}

const SIMPLE_UNITS: [&str; 45] = [
    "acre", "bit", "byte", "celsius", "centimeter", "day", "degree", "fahrenheit", "fluid-ounce",
    "foot", "gallon", "gigabit", "gigabyte", "gram", "hectare", "hour", "inch", "kilobit",
    "kilobyte", "kilogram", "kilometer", "liter", "megabit", "megabyte", "meter", "microsecond",
    "mile", "mile-scandinavian", "milliliter", "millimeter", "millisecond", "minute", "month",
    "nanosecond", "ounce", "percent", "petabyte", "pound", "second", "stone", "terabit",
    "terabyte", "week", "yard", "year",
];

fn is_unit(u: &str) -> bool {
    if SIMPLE_UNITS.contains(&u) {
        return true;
    }
    match u.split_once("-per-") {
        Some((a, b)) => SIMPLE_UNITS.contains(&a) && SIMPLE_UNITS.contains(&b),
        None => false,
    }
}

fn currency_digits(code: &str) -> u32 {
    CURRENCIES
        .iter()
        .find(|c| c.code == code)
        .map_or(2, |c| u32::from(c.digits))
}

const INCREMENTS: [f64; 15] = [
    1.0, 2.0, 5.0, 10.0, 20.0, 25.0, 50.0, 100.0, 200.0, 250.0, 500.0, 1000.0, 2000.0, 2500.0,
    5000.0,
];

fn digit_option(raw: &Spec, name: &str, min: f64, max: f64) -> Result<Option<u32>, Fault> {
    match raw.get(name) {
        None => Ok(None),
        Some(v) => {
            let n = super::parse_f64(v);
            if !(n >= min && n <= max) {
                return range(&format!("{name} value is out of range."));
            }
            Ok(Some(libm_floor(n) as u32))
        }
    }
}

fn libm_floor(n: f64) -> f64 {
    let t = n as i64 as f64;
    if t > n { t - 1.0 } else { t }
}

pub(super) struct Digits {
    pub min_int: u32,
    pub min_fd: u32,
    pub max_fd: u32,
    pub min_sd: u32,
    pub max_sd: u32,
    pub kind: &'static str,
    pub priority: &'static str,
}

pub(super) fn digit_options(
    raw: &Spec,
    out: &mut Spec,
    fd_default: (u32, u32),
    compact: bool,
) -> Result<Digits, Fault> {
    let min_int = digit_option(raw, "minimumIntegerDigits", 1.0, 21.0)?.unwrap_or(1);
    let (mnfd, mxfd) = (raw.get("minimumFractionDigits"), raw.get("maximumFractionDigits"));
    let (mnsd, mxsd) = (raw.get("minimumSignificantDigits"), raw.get("maximumSignificantDigits"));
    let inc = match raw.get("roundingIncrement") {
        None => 1.0,
        Some(v) => {
            let n = super::parse_f64(v);
            if !(1.0..=5000.0).contains(&n) {
                return range("roundingIncrement value is out of range.");
            }
            let n = libm_floor(n);
            if !INCREMENTS.contains(&n) {
                return range("roundingIncrement value is out of range.");
            }
            n
        }
    };
    let mode = raw.choose(
        "roundingMode",
        &["ceil", "floor", "expand", "trunc", "halfCeil", "halfFloor", "halfExpand", "halfTrunc", "halfEven"],
        "halfExpand",
        "Intl.NumberFormat",
    )?;
    let priority = raw.choose("roundingPriority", &["auto", "morePrecision", "lessPrecision"], "auto", "Intl.NumberFormat")?;
    let tzd = raw.choose("trailingZeroDisplay", &["auto", "stripIfInteger"], "auto", "Intl.NumberFormat")?;
    let has_sd = mnsd.is_some() || mxsd.is_some();
    let has_fd = mnfd.is_some() || mxfd.is_some();
    let mut need_sd = true;
    let mut need_fd = true;
    if priority == "auto" {
        need_sd = has_sd;
        if need_sd || (!has_fd && compact) {
            need_fd = false;
        }
    }
    let mut d = Digits { min_int, min_fd: 0, max_fd: 0, min_sd: 1, max_sd: 21, kind: "fractionDigits", priority };
    if need_sd && has_sd {
        d.min_sd = digit_option(raw, "minimumSignificantDigits", 1.0, 21.0)?.unwrap_or(1);
        d.max_sd = digit_option(raw, "maximumSignificantDigits", f64::from(d.min_sd), 21.0)?.unwrap_or(21);
    }
    if need_fd {
        if has_fd {
            let lo = digit_option(raw, "minimumFractionDigits", 0.0, 100.0)?;
            let hi = digit_option(raw, "maximumFractionDigits", 0.0, 100.0)?;
            match (lo, hi) {
                (None, Some(h)) => {
                    d.min_fd = fd_default.0.min(h);
                    d.max_fd = h;
                }
                (Some(l), None) => {
                    d.min_fd = l;
                    d.max_fd = fd_default.1.max(l);
                }
                (Some(l), Some(h)) => {
                    if l > h {
                        return range("maximumFractionDigits value is out of range.");
                    }
                    d.min_fd = l;
                    d.max_fd = h;
                }
                (None, None) => {}
            }
        } else {
            d.min_fd = fd_default.0;
            d.max_fd = fd_default.1;
        }
    }
    if !need_sd && !need_fd {
        d.min_fd = 0;
        d.max_fd = 0;
        d.min_sd = 1;
        d.max_sd = 2;
        d.kind = "morePrecision";
        d.priority = "morePrecision";
    } else if priority == "auto" {
        d.kind = if has_sd { "significantDigits" } else { "fractionDigits" };
    } else {
        d.kind = priority;
    }
    if inc != 1.0 {
        if d.kind != "fractionDigits" {
            return type_err("roundingIncrement is only valid with fraction digits rounding");
        }
        if d.max_fd != d.min_fd {
            return range("maximumFractionDigits value is out of range.");
        }
    }
    out.num("minimumIntegerDigits", min_int);
    if d.kind != "significantDigits" {
        out.num("minimumFractionDigits", d.min_fd);
        out.num("maximumFractionDigits", d.max_fd);
    }
    if d.kind != "fractionDigits" {
        out.num("minimumSignificantDigits", d.min_sd);
        out.num("maximumSignificantDigits", d.max_sd);
    }
    out.later("roundingIncrement", &format!("#{inc}"));
    out.later("roundingMode", mode);
    out.later("roundingPriority", d.priority);
    out.later("trailingZeroDisplay", tzd);
    Ok(d)
}

pub(super) fn resolve(locale: &str, raw: &Spec) -> Result<Spec, Fault> {
    let mut out = Spec::default();
    out.put("locale", locale);
    out.put("numberingSystem", "latn");
    let n = "Intl.NumberFormat";
    let style = raw.choose("style", &["decimal", "percent", "currency", "unit"], "decimal", n)?;
    let currency = match raw.get("currency") {
        Some(c) => {
            if c.len() != 3 || !c.bytes().all(|b| b.is_ascii_alphabetic()) {
                return range(&format!("Invalid currency code : {c}"));
            }
            Some(c.to_ascii_uppercase())
        }
        None => None,
    };
    let cd = raw.choose("currencyDisplay", &["code", "symbol", "narrowSymbol", "name"], "symbol", n)?;
    let cs = raw.choose("currencySign", &["standard", "accounting"], "standard", n)?;
    let unit = raw.get("unit").map(String::from);
    if let Some(u) = &unit
        && !is_unit(u)
    {
        return range(&format!("Invalid unit argument for Intl.NumberFormat() '{u}'"));
    }
    let ud = raw.choose("unitDisplay", &["short", "narrow", "long"], "short", n)?;
    out.put("style", style);
    let notation = raw.choose("notation", &["standard", "scientific", "engineering", "compact"], "standard", n)?;
    let fd = match style {
        "currency" => {
            let Some(c) = &currency else {
                return type_err("Currency code is required with currency style.");
            };
            out.put("currency", c);
            out.put("currencyDisplay", cd);
            out.put("currencySign", cs);
            let d = if notation == "standard" { currency_digits(c) } else { 0 };
            (d, d)
        }
        "unit" => {
            let Some(u) = &unit else {
                return type_err("Invalid unit argument for Intl.NumberFormat() ''");
            };
            out.put("unit", u);
            out.put("unitDisplay", ud);
            (0, 3)
        }
        "percent" => (0, 0),
        _ => (0, 3),
    };
    let compact = notation == "compact";
    digit_options(raw, &mut out, fd, compact)?;
    let cdisp = raw.choose("compactDisplay", &["short", "long"], "short", n)?;
    let grouping = match raw.get("useGrouping") {
        None => if compact { "min2" } else { "auto" },
        Some("false") => "!f",
        Some("true") => "always",
        Some(g) => {
            let g = ["min2", "auto", "always"].into_iter().find(|x| *x == g);
            match g {
                Some(g) => g,
                None => return range(&format!("Value {} out of range for {n} options property useGrouping", raw.get("useGrouping").unwrap_or(""))),
            }
        }
    };
    out.put("useGrouping", grouping);
    out.put("notation", notation);
    if compact {
        out.put("compactDisplay", cdisp);
    }
    let sd = raw.choose("signDisplay", &["auto", "never", "always", "exceptZero", "negative"], "auto", n)?;
    out.put("signDisplay", sd);
    out.flush_later();
    Ok(out)
}

#[derive(Clone)]
pub(super) struct Dec {
    pub neg: bool,
    pub digits: Vec<u8>,
    pub exp: i32,
}

pub(super) enum Num {
    Nan,
    Inf(bool),
    Dec(Dec),
}

impl Dec {
    fn trim(&mut self) {
        while self.digits.last() == Some(&0) {
            self.digits.pop();
        }
        let lead = self.digits.iter().take_while(|d| **d == 0).count();
        if lead > 0 {
            self.digits.drain(..lead);
            self.exp -= lead as i32;
        }
        if self.digits.is_empty() {
            self.exp = 0;
        }
    }
    pub fn is_zero(&self) -> bool {
        self.digits.is_empty()
    }
}

pub(super) fn parse_decimal(s: &str) -> Num {
    let t = s.trim_matches(|c: char| c.is_whitespace());
    let (neg, body) = match t.as_bytes().first() {
        Some(b'-') => (true, &t[1..]),
        Some(b'+') => (false, &t[1..]),
        _ => (false, t),
    };
    if body == "Infinity" {
        return Num::Inf(neg);
    }
    let (mant, e) = match body.find(['e', 'E']) {
        Some(i) => (&body[..i], body[i + 1..].parse::<i32>().ok()),
        None => (body, Some(0)),
    };
    let Some(e) = e else { return Num::Nan };
    let (ip, fp) = mant.split_once('.').unwrap_or((mant, ""));
    if (ip.is_empty() && fp.is_empty()) || !ip.bytes().chain(fp.bytes()).all(|b| b.is_ascii_digit()) {
        return Num::Nan;
    }
    let mut d = Dec {
        neg,
        digits: ip.bytes().chain(fp.bytes()).map(|b| b - b'0').collect(),
        exp: ip.len() as i32 + e,
    };
    d.trim();
    Num::Dec(d)
}

pub(super) fn from_f64(x: f64) -> Num {
    if x.is_nan() {
        return Num::Nan;
    }
    if x.is_infinite() {
        return Num::Inf(x < 0.0);
    }
    let s = crate::num::to_string(x.abs());
    match parse_decimal(s.as_str()) {
        Num::Dec(mut d) => {
            d.neg = x.is_sign_negative();
            Num::Dec(d)
        }
        other => other,
    }
}

fn round_up(digits: &[u8], keep: i32, neg: bool, mode: &str) -> bool {
    let keep = keep.max(0) as usize;
    let rest = digits.get(keep..).unwrap_or(&[]);
    if rest.iter().all(|d| *d == 0) {
        return false;
    }
    let first = rest[0];
    let half_more = first > 5 || (first == 5 && rest[1..].iter().any(|d| *d != 0));
    let exact_half = first == 5 && rest[1..].iter().all(|d| *d == 0);
    let last_odd = keep > 0 && digits.get(keep - 1).is_some_and(|d| d % 2 == 1);
    match mode {
        "ceil" => !neg,
        "floor" => neg,
        "expand" => true,
        "trunc" => false,
        _ if half_more => true,
        _ if !exact_half => false,
        "halfCeil" => !neg,
        "halfFloor" => neg,
        "halfTrunc" => false,
        "halfEven" => last_odd,
        _ => true,
    }
}

fn round_at(d: &Dec, keep: i32, mode: &str) -> Dec {
    if keep < 0 {
        let mut digits = alloc::vec![0u8; keep.unsigned_abs() as usize];
        digits.extend_from_slice(&d.digits);
        let padded = Dec { neg: d.neg, digits, exp: d.exp - keep };
        return round_at(&padded, 0, mode);
    }
    let mut r = d.clone();
    if keep >= r.digits.len() as i32 {
        return r;
    }
    let up = round_up(&d.digits, keep, d.neg, mode);
    r.digits.truncate(keep as usize);
    if up {
        let mut i = r.digits.len();
        loop {
            if i == 0 {
                r.digits.insert(0, 1);
                r.exp += 1;
                break;
            }
            i -= 1;
            if r.digits[i] == 9 {
                r.digits[i] = 0;
            } else {
                r.digits[i] += 1;
                break;
            }
        }
    }
    r.trim();
    r
}

pub(super) struct Rounded {
    pub dec: Dec,
    pub min_frac: i32,
    pub min_int: i32,
}

pub(super) fn round(d: &Dec, g: &Digits, mode: &str, strip: bool) -> Rounded {
    let by_fd = || {
        let r = round_at(d, d.exp + g.max_fd as i32, mode);
        (r, g.min_fd as i32)
    };
    let by_sd = || {
        let r = round_at(d, g.max_sd as i32, mode);
        let lead = if r.is_zero() { 1 } else { r.exp };
        (r, g.min_sd as i32 - lead)
    };
    let (dec, mut min_frac) = match g.kind {
        "fractionDigits" => by_fd(),
        "significantDigits" => by_sd(),
        k => {
            let (a, b) = (by_fd(), by_sd());
            let fa = a.0.digits.len() as i32 - a.0.exp;
            let fb = b.0.digits.len() as i32 - b.0.exp;
            let more = fb > fa || (fb == fa && b.1 > a.1);
            if (k == "morePrecision") == more { b } else { a }
        }
    };
    if strip && dec.digits.len() as i32 <= dec.exp.max(0) {
        min_frac = 0;
    }
    Rounded { dec, min_frac: min_frac.max(0), min_int: g.min_int as i32 }
}

fn group(int: &str, loc: &NumLocale, mode: &str) -> Vec<String> {
    let n = int.len();
    let min = match mode {
        "!f" => return alloc::vec![int.to_string()],
        "always" => 4,
        "min2" => 5,
        _ => if loc.min2 { 5 } else { 4 },
    };
    if n < min {
        return alloc::vec![int.to_string()];
    }
    let mut groups = Vec::new();
    let mut end = n;
    let mut size = 3;
    while end > size {
        groups.push(int[end - size..end].to_string());
        end -= size;
        if loc.indian {
            size = 2;
        }
    }
    groups.push(int[..end].to_string());
    groups.reverse();
    groups
}

fn digits_parts(r: &Rounded, loc: &NumLocale, grouping: &str, out: &mut Parts) {
    let d = &r.dec;
    let mut int = String::new();
    let mut frac = String::new();
    for i in 0..d.exp.max(0) {
        int.push((b'0' + d.digits.get(i as usize).copied().unwrap_or(0)) as char);
    }
    while (int.len() as i32) < r.min_int {
        int.insert(0, '0');
    }
    let nfrac = (d.digits.len() as i32 - d.exp).max(r.min_frac);
    for i in 0..nfrac {
        let at = d.exp + i;
        let c = if at < 0 { 0 } else { d.digits.get(at as usize).copied().unwrap_or(0) };
        frac.push((b'0' + c) as char);
    }
    let groups = group(&int, loc, grouping);
    for (i, g) in groups.iter().enumerate() {
        if i > 0 {
            out.push("group", loc.grp);
        }
        out.push("integer", g);
    }
    if !frac.is_empty() {
        out.push("decimal", loc.dec);
        out.push("fraction", &frac);
    }
}

pub(super) fn plural_of(lang: &str, r: &Rounded, ordinal: bool) -> &'static str {
    let d = &r.dec;
    let visible = (d.digits.len() as i32 - d.exp).max(r.min_frac).max(0) as u32;
    let mut int: u64 = 0;
    for i in 0..d.exp.max(0) {
        int = int.saturating_mul(10).saturating_add(u64::from(d.digits.get(i as usize).copied().unwrap_or(0)));
    }
    let mut fraction: u64 = 0;
    for i in 0..visible.min(18) as i32 {
        let at = d.exp + i;
        let c = if at < 0 { 0 } else { d.digits.get(at as usize).copied().unwrap_or(0) };
        fraction = fraction * 10 + u64::from(c);
    }
    plural_en_like(lang, int, visible, fraction, ordinal)
}

pub(super) struct Formatted {
    pub parts: Parts,
}

fn sign_of(sd: &str, neg: bool, zero: bool) -> Option<bool> {
    match sd {
        "never" => None,
        "always" => Some(neg),
        "exceptZero" => if zero { None } else { Some(neg) },
        "negative" => if neg && !zero { Some(true) } else { None },
        _ => if neg { Some(true) } else { None },
    }
}

fn affix_parts(pattern: &str, sign: Option<bool>, body: &Parts, sym: Option<(&'static str, &str)>, loc: &NumLocale, out: &mut Parts) {
    let mut lit = String::new();
    let flush = |lit: &mut String, out: &mut Parts| {
        if !lit.is_empty() {
            out.push("literal", lit);
            lit.clear();
        }
    };
    let chars: Vec<char> = pattern.chars().collect();
    for (i, c) in chars.iter().enumerate() {
        match c {
            '#' => {
                flush(&mut lit, out);
                out.extend(body);
            }
            '-' => {
                flush(&mut lit, out);
                match sign {
                    Some(true) => out.push("minusSign", loc.minus),
                    Some(false) => out.push("plusSign", "+"),
                    None => {}
                }
            }
            '%' => {
                flush(&mut lit, out);
                out.push("percentSign", "%");
            }
            '\u{a4}' => {
                flush(&mut lit, out);
                if let Some((ty, s)) = sym {
                    let next_digit = chars.get(i + 1) == Some(&'#');
                    let prev_digit = i > 0 && chars[i - 1] == '#';
                    let letter_end = s.chars().last().is_some_and(|c| c.is_alphabetic());
                    let letter_start = s.chars().next().is_some_and(|c| c.is_alphabetic());
                    if prev_digit && letter_start {
                        out.push("literal", "\u{a0}");
                    }
                    out.push(ty, s);
                    if next_digit && letter_end {
                        out.push("literal", "\u{a0}");
                    }
                }
            }
            c => lit.push(*c),
        }
    }
    flush(&mut lit, out);
}

fn with_sign(p: &str) -> String {
    if p.contains('-') { p.to_string() } else { format!("-{p}") }
}

pub(super) fn format(spec: &Spec, x: Num) -> Formatted {
    let locale = spec.get("locale").unwrap_or("en-US");
    let lang = locale.split('-').next().unwrap_or("en");
    let loc = num_locale(locale);
    let style = spec.get("style").unwrap_or("decimal");
    let notation = spec.get("notation").unwrap_or("standard");
    let grouping = spec.get("useGrouping").unwrap_or("auto");
    let sd = spec.get("signDisplay").unwrap_or("auto");
    let mode = spec.get("roundingMode").unwrap_or("halfExpand");
    let strip = spec.get("trailingZeroDisplay") == Some("stripIfInteger");
    let g = Digits {
        min_int: spec.int("minimumIntegerDigits", 1),
        min_fd: spec.int("minimumFractionDigits", 0),
        max_fd: spec.int("maximumFractionDigits", 3),
        min_sd: spec.int("minimumSignificantDigits", 1),
        max_sd: spec.int("maximumSignificantDigits", 21),
        kind: match (spec.get("minimumFractionDigits").is_some(), spec.get("minimumSignificantDigits").is_some()) {
            (true, true) => if spec.get("roundingPriority") == Some("lessPrecision") { "lessPrecision" } else { "morePrecision" },
            (false, true) => "significantDigits",
            _ => "fractionDigits",
        },
        priority: "auto",
    };
    let mut body = Parts::default();
    let mut neg = false;
    let mut zero = false;
    let mut plural = "other";
    let mut compact_affix: Option<String> = None;
    match x {
        Num::Nan => {
            body.push("nan", loc.nan);
        }
        Num::Inf(n) => {
            neg = n;
            body.push("infinity", "\u{221e}");
        }
        Num::Dec(mut d) => {
            neg = d.neg;
            if style == "percent" && !d.is_zero() {
                d.exp += 2;
            }
            match notation {
                "scientific" | "engineering" => {
                    let mut e = if d.is_zero() { 0 } else { d.exp - 1 };
                    if notation == "engineering" {
                        e -= e.rem_euclid(3);
                    }
                    let mut m = d.clone();
                    m.exp -= e;
                    let mut r = round(&m, &g, mode, strip);
                    if !r.dec.is_zero() && r.dec.exp - 1 > (if notation == "engineering" { 2 } else { 0 }) {
                        let step = if notation == "engineering" { 3 } else { 1 };
                        e += step;
                        m.exp -= step;
                        r = round(&m, &g, mode, strip);
                    }
                    zero = r.dec.is_zero();
                    digits_parts(&r, loc, grouping, &mut body);
                    body.push("exponentSeparator", "E");
                    if e < 0 {
                        body.push("exponentMinusSign", loc.minus);
                    }
                    body.push("exponentInteger", &e.unsigned_abs().to_string());
                }
                "compact" => {
                    let long = spec.get("compactDisplay") == Some("long");
                    let table = COMPACT
                        .iter()
                        .find(|(l, lg, _)| *l == lang && *lg == long)
                        .or_else(|| COMPACT.iter().find(|(l, lg, _)| *l == "en" && *lg == long))
                        .map(|t| &t.2);
                    let mut mag = if d.is_zero() { 0 } else { d.exp - 1 };
                    let mut r;
                    let mut pat;
                    loop {
                        pat = None;
                        let mut m = d.clone();
                        if mag >= 3
                            && let Some(t) = table
                        {
                            let i = (mag.min(14) - 3) as usize;
                            let [one, other] = t[i];
                            if !other.is_empty() {
                                let zeros = other.chars().filter(|c| *c == '0').count() as i32;
                                let shift = mag.min(14) - zeros + 1;
                                m.exp -= shift;
                                pat = Some((one, other));
                            }
                        }
                        r = round(&m, &g, mode, strip);
                        let new_mag = if r.dec.is_zero() { 0 } else { r.dec.exp - 1 + (d.exp - m.exp) };
                        if new_mag > mag && mag < 14 {
                            mag = new_mag;
                            continue;
                        }
                        break;
                    }
                    zero = r.dec.is_zero();
                    plural = plural_of(lang, &r, false);
                    if let Some((one, other)) = pat {
                        let p = if plural == "one" { one } else { other };
                        compact_affix = Some(p.to_string());
                    }
                    digits_parts(&r, loc, grouping, &mut body);
                }
                _ => {
                    let r = round(&d, &g, mode, strip);
                    zero = r.dec.is_zero();
                    plural = plural_of(lang, &r, false);
                    digits_parts(&r, loc, grouping, &mut body);
                }
            }
        }
    }
    if let Some(p) = compact_affix {
        let first = p.find('0').unwrap_or(0);
        let last = p.rfind('0').map_or(0, |i| i + 1);
        let (pre, post) = (&p[..first], &p[last..]);
        let mut nb = Parts::default();
        let core = pre.trim_end();
        if !core.is_empty() {
            nb.push("compact", core);
        }
        if core.len() < pre.len() {
            nb.push("literal", &pre[core.len()..]);
        }
        nb.extend(&body);
        let core = post.trim_start();
        if core.len() < post.len() {
            nb.push("literal", &post[..post.len() - core.len()]);
        }
        if !core.is_empty() {
            nb.push("compact", core);
        }
        body = nb;
    }
    let sign = if x_is_nan(&body) { None } else { sign_of(sd, neg, zero) };
    let mut out = Parts::default();
    match style {
        "currency" => {
            let code = spec.get("currency").unwrap_or("USD");
            let cd = spec.get("currencyDisplay").unwrap_or("symbol");
            let entry = CURRENCIES.iter().find(|c| c.code == code);
            if cd == "name" {
                let name = entry.map_or(code, |c| if plural == "one" { c.one } else { c.other });
                let p = if sign.is_some() { "-# \u{a4}" } else { "# \u{a4}" };
                affix_parts(p, sign, &body, Some(("currency", name)), loc, &mut out);
                return Formatted { parts: out };
            }
            let sym: &str = match cd {
                "code" => code,
                "narrowSymbol" => entry.map_or(code, |c| c.narrow),
                _ => CURRENCY_SYMBOLS
                    .iter()
                    .find(|(l, c, _)| *c == code && (*l == locale || *l == lang))
                    .map(|t| t.2)
                    .or(entry.map(|c| c.symbol))
                    .unwrap_or(code),
            };
            let acct = spec.get("currencySign") == Some("accounting");
            let pat = match sign {
                Some(true) if acct => loc.acct.to_string(),
                Some(_) => loc.neg_cur.to_string(),
                None => loc.cur.to_string(),
            };
            let parenthesized = acct && sign == Some(true);
            let pat = if sign.is_some() && !parenthesized { with_sign(&pat) } else { pat };
            let sign = if parenthesized { None } else { sign };
            affix_parts(&pat, sign, &body, Some(("currency", sym)), loc, &mut out);
        }
        "percent" => {
            let pat = if sign.is_some() { with_sign(loc.neg_pct) } else { loc.pct.to_string() };
            affix_parts(&pat, sign, &body, None, loc, &mut out);
        }
        "unit" => {
            let unit = spec.get("unit").unwrap_or("");
            let disp = match spec.get("unitDisplay") {
                Some("long") => 1,
                Some("narrow") => 2,
                _ => 0,
            };
            let pl = usize::from(plural != "one");
            let pattern = unit_pattern(unit, disp, pl);
            let mut num = Parts::default();
            affix_parts(if sign.is_some() { "-#" } else { "#" }, sign, &body, None, loc, &mut num);
            let (pre, post) = pattern.split_once("{0}").unwrap_or(("", &pattern));
            for (s, is_pre) in [(pre, true), (post, false)] {
                if !is_pre {
                    out.extend(&num);
                }
                if s.is_empty() {
                    continue;
                }
                let ws_l = s.len() - s.trim_start().len();
                let ws_r = s.len() - s.trim_end().len();
                if ws_l > 0 { out.push("literal", &s[..ws_l]); }
                let core = s.trim();
                if !core.is_empty() { out.push("unit", core); }
                if ws_r > 0 && core.len() + ws_l < s.len() { out.push("literal", &s[s.len() - ws_r..]); }
            }
        }
        _ => {
            affix_parts(if sign.is_some() { "-#" } else { "#" }, sign, &body, None, loc, &mut out);
        }
    }
    Formatted { parts: out }
}

fn x_is_nan(p: &Parts) -> bool {
    p.items.iter().any(|(t, _)| *t == "nan")
}

fn unit_pattern(unit: &str, disp: usize, pl: usize) -> String {
    let simple = |u: &str| UNITS.iter().find(|(n, _)| *n == u).map(|(_, p)| p[disp][pl]);
    if let Some(p) = simple(unit) {
        return p.to_string();
    }
    if let Some((_, _, _, p)) = UNIT_PER.iter().find(|(u, d, n, _)| *u == unit && usize::from(*d) == disp && usize::from(*n) == pl) {
        return p.to_string();
    }
    if let Some((a, b)) = unit.split_once("-per-")
        && let Some(x) = simple(a)
        && let Some((_, per)) = UNIT_PER_PATTERNS.iter().find(|(n, _)| *n == b)
    {
        return per[disp][pl].replacen("{0}", x, 1);
    }
    "{0}".to_string()
}
