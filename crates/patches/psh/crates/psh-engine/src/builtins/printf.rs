//! `printf [-v var] format [arguments]`: the format is reused until the arguments are
//! consumed; conversions `d i o u x X c s b q f F e E g G %`.

use crate::escape::{Dialect, decode};
use crate::state::{R, Vp, is_name};

use super::{out, shell_quote};

fn s(b: &[u8]) -> std::borrow::Cow<'_, str> {
    String::from_utf8_lossy(b)
}

pub(super) async fn printf(vp: &mut Vp, args: &[Vec<u8>]) -> R {
    let mut i = 1;
    let mut var: Option<String> = None;
    while i < args.len() {
        match &args[i][..] {
            b"--" => {
                i += 1;
                break;
            }
            a if a.starts_with(b"-v") => {
                // `-v NAME` or `-vNAME`.
                let attached = a.len() > 2;
                let Some(n) = (if attached {
                    Some(&a[2..])
                } else {
                    args.get(i + 1).map(Vec::as_slice)
                }) else {
                    vp.error("printf: -v: option requires an argument").await;
                    vp.error_raw(b"printf: usage: printf [-v var] format [arguments]\n")
                        .await;
                    return Ok(2);
                };
                if !valid_target(n) {
                    vp.error(&format!("printf: `{}': not a valid identifier", s(n)))
                        .await;
                    return Ok(2);
                }
                var = Some(s(n).into_owned());
                i += if attached { 1 } else { 2 };
            }
            a if a.len() > 1 && a[0] == b'-' => {
                vp.error(&format!("printf: {}: invalid option", s(&a[..2])))
                    .await;
                vp.error_raw(b"printf: usage: printf [-v var] format [arguments]\n")
                    .await;
                return Ok(2);
            }
            _ => break,
        }
    }
    let Some(format) = args.get(i) else {
        vp.error_raw(b"printf: usage: printf [-v var] format [arguments]\n")
            .await;
        return Ok(2);
    };
    // `%(…)T` needs the time zone, read before formatting.
    let zone = if format.windows(2).any(|w| w == b"%(") {
        Some(super::strftime::zone(vp).await)
    } else {
        None
    };
    let mut f = Formatter {
        zone,
        now: vp.host().now().as_secs() as i64,
        args: &args[i + 1..],
        next: 0,
        out: Vec::new(),
        errors: Vec::new(),
        stop: false,
        utf8: vp.utf8(),
    };
    // bash reports a pass's errors as they happen and writes its output at the end of
    // each pass through the format.
    let mut status = 0;
    let mut write_st = 0;
    loop {
        let before = f.next;
        f.run(format);
        status |= i32::from(!f.errors.is_empty());
        for e in std::mem::take(&mut f.errors) {
            vp.error(&format!("printf: {e}")).await;
        }
        if var.is_none() && !f.out.is_empty() && write_st == 0 {
            write_st = out(vp, "printf", &std::mem::take(&mut f.out)).await?;
        }
        if f.stop || f.next >= f.args.len() || f.next == before {
            break;
        }
    }
    match var {
        Some(n) => {
            // bash stores the result as a C string: it ends at the first NUL.
            if let Some(end) = f.out.iter().position(|&b| b == 0) {
                f.out.truncate(end);
            }
            // `NAME` or `NAME[SUB]`, with the variable's attributes.
            let r = match n.split_once('[') {
                Some((base, sub)) => {
                    let sub = sub.strip_suffix(']').unwrap_or(sub);
                    let sub = crate::expand::expand_sub_text(vp, sub.as_bytes()).await?;
                    vp.assign_elem(base, &sub, f.out, false)
                }
                None => vp.assign(&n, f.out),
            };
            if let Err(msg) = r {
                vp.error(&msg).await;
                return Ok(1);
            }
            Ok(status)
        }
        None => Ok(if write_st != 0 { write_st } else { status }),
    }
}

/// `%#q`: always single quotes (`'it'\''s'`), unless control characters need `$'…'`.
fn single_quote(v: &[u8], utf8: bool) -> Vec<u8> {
    if super::set::needs_ansi(v, utf8) {
        return shell_quote(v, utf8);
    }
    let mut out = vec![b'\''];
    for &b in v {
        if b == b'\'' {
            out.extend_from_slice(b"'\\''");
        } else {
            out.push(b);
        }
    }
    out.push(b'\'');
    out
}

/// `-v`'s target: a name, or a name with a non-empty subscript ending the word.
fn valid_target(n: &[u8]) -> bool {
    match n.iter().position(|&b| b == b'[') {
        None => is_name(n),
        Some(open) => {
            is_name(&n[..open]) && n.len() > open + 2 && subscript_end(n, open) == Some(n.len() - 1)
        }
    }
}

fn subscript_end(n: &[u8], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut i = open;
    while i < n.len() {
        match n[i] {
            b'\\' => i += 1,
            b'\'' => i += n[i + 1..].iter().position(|&b| b == b'\'')? + 1,
            b'"' => {
                i += 1;
                while *n.get(i)? != b'"' {
                    i += if n[i] == b'\\' { 2 } else { 1 };
                }
            }
            b'`' => i += n[i + 1..].iter().position(|&b| b == b'`')? + 1,
            b'$' if matches!(n.get(i + 1), Some(b'(' | b'{')) => {
                let (o, c) = if n[i + 1] == b'(' {
                    (b'(', b')')
                } else {
                    (b'{', b'}')
                };
                let mut level = 0usize;
                i += 1;
                loop {
                    match *n.get(i)? {
                        b if b == o => level += 1,
                        b if b == c => {
                            level -= 1;
                            if level == 0 {
                                break;
                            }
                        }
                        _ => {}
                    }
                    i += 1;
                }
            }
            b'[' => depth += 1,
            b']' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

struct Formatter<'a> {
    zone: Option<super::strftime::Zone>,
    /// The time for `%(…)T` with `-1`, `-2` or no argument.
    now: i64,
    args: &'a [Vec<u8>],
    next: usize,
    out: Vec<u8>,
    errors: Vec<String>,
    stop: bool,
    utf8: bool,
}

#[derive(Default)]
struct Spec {
    left: bool,
    zero: bool,
    plus: bool,
    space: bool,
    alt: bool,
    width: Option<usize>,
    prec: Option<usize>,
}

impl Formatter<'_> {
    fn arg(&mut self) -> Option<&[u8]> {
        let a = self.args.get(self.next);
        if a.is_some() {
            self.next += 1;
        }
        a.map(Vec::as_slice)
    }

    /// An integer argument for a signed conversion (also `*` widths): clamped to
    /// the range of `i64` with an error, as `strtoimax` does.
    fn num_arg(&mut self) -> i64 {
        // A missing or empty argument is 0 (`strtoimax` stops at once, at the end).
        let Some(a) = self.arg().map(<[u8]>::to_vec).filter(|a| !a.is_empty()) else {
            return 0;
        };
        let n = parse_num(&a, self.utf8);
        if !n.ok {
            self.errors.push(format!("{}: invalid number", s(&a)));
        }
        let limit = if n.neg {
            1u128 << 63
        } else {
            (1u128 << 63) - 1
        };
        if n.overflow || n.mag > limit {
            self.errors
                .push(format!("{}: Numerical result out of range", s(&a)));
            return if n.neg { i64::MIN } else { i64::MAX };
        }
        if n.neg {
            (n.mag as i128).wrapping_neg() as i64
        } else {
            n.mag as i64
        }
    }

    /// An integer argument for an unsigned conversion: negative values wrap around,
    /// as `strtoumax` does.
    fn unsigned_arg(&mut self) -> u64 {
        let Some(a) = self.arg().map(<[u8]>::to_vec).filter(|a| !a.is_empty()) else {
            return 0;
        };
        let n = parse_num(&a, self.utf8);
        if !n.ok {
            self.errors.push(format!("{}: invalid number", s(&a)));
        }
        if n.overflow {
            self.errors
                .push(format!("{}: Numerical result out of range", s(&a)));
            return u64::MAX;
        }
        let v = n.mag as u64;
        if n.neg { v.wrapping_neg() } else { v }
    }

    fn float_arg(&mut self) -> f64 {
        match self.arg() {
            None => 0.0,
            Some(a) => {
                let a = a.to_vec();
                if let Some(v) = char_code(&a, self.utf8) {
                    return f64::from(v);
                }
                let t = s(&a);
                let t = t.trim();
                match t.parse::<f64>() {
                    Ok(v) => v,
                    Err(_) => {
                        if a.is_empty() {
                            return 0.0;
                        }
                        // What strtold converts of it, with an error for the rest
                        // (`2228e9x` is 2228e9).
                        let (v, used) = float_prefix(t.as_bytes()).unwrap_or((0.0, 0));
                        if used < t.len() {
                            self.errors.push(format!("{}: invalid number", s(&a)));
                        }
                        v
                    }
                }
            }
        }
    }

    fn run(&mut self, fmt: &[u8]) {
        let mut i = 0;
        while i < fmt.len() && !self.stop {
            let c = fmt[i];
            if c == b'\\' {
                // One escape at a time, so `\c` is literal here and octal is `\nnn`.
                let mut j = i + 1;
                if j < fmt.len() {
                    j += 1;
                    let lead = fmt[i + 1];
                    let max = match lead {
                        b'0'..=b'7' => 2,
                        b'x' => 2,
                        b'u' => 4,
                        b'U' => 8,
                        _ => 0,
                    };
                    let mut n = 0;
                    while n < max && j < fmt.len() && fmt[j].is_ascii_hexdigit() {
                        if matches!(lead, b'0'..=b'7') && !(b'0'..=b'7').contains(&fmt[j]) {
                            break;
                        }
                        j += 1;
                        n += 1;
                    }
                }
                self.out
                    .extend_from_slice(&decode(&fmt[i..j], Dialect::Printf).bytes);
                i = j;
                continue;
            }
            if c != b'%' {
                self.out.push(c);
                i += 1;
                continue;
            }
            i += 1;
            if i >= fmt.len() {
                self.out.push(b'%');
                break;
            }
            if fmt[i] == b'%' {
                self.out.push(b'%');
                i += 1;
                continue;
            }
            let start = i - 1;
            let mut spec = Spec::default();
            while i < fmt.len() {
                match fmt[i] {
                    b'-' => spec.left = true,
                    b'0' => spec.zero = true,
                    b'+' => spec.plus = true,
                    b' ' => spec.space = true,
                    b'#' => spec.alt = true,
                    _ => break,
                }
                i += 1;
            }
            if i < fmt.len() && fmt[i] == b'*' {
                let w = self.num_arg();
                if w < 0 {
                    spec.left = true;
                }
                spec.width = Some(w.unsigned_abs() as usize);
                i += 1;
            } else {
                let st = i;
                while i < fmt.len() && fmt[i].is_ascii_digit() {
                    i += 1;
                }
                if i > st {
                    spec.width = s(&fmt[st..i]).parse().ok();
                }
            }
            if i < fmt.len() && fmt[i] == b'.' {
                i += 1;
                if i < fmt.len() && fmt[i] == b'*' {
                    spec.prec = Some(self.num_arg().max(0) as usize);
                    i += 1;
                } else {
                    let st = i;
                    while i < fmt.len() && fmt[i].is_ascii_digit() {
                        i += 1;
                    }
                    spec.prec = Some(s(&fmt[st..i]).parse().unwrap_or(0));
                }
            }
            if fmt.get(i) == Some(&b'(') {
                // `%(strftime format)T`
                let Some(close) = fmt[i..].iter().position(|&b| b == b')') else {
                    self.errors
                        .push(format!("`{}': missing format character", s(&fmt[start..])));
                    self.stop = true;
                    break;
                };
                // An empty format is `%X` (bash).
                let tfmt = match &fmt[i + 1..i + close] {
                    [] => b"%X".to_vec(),
                    t => t.to_vec(),
                };
                i += close + 1;
                if fmt.get(i) != Some(&b'T') {
                    let shown = fmt
                        .get(i)
                        .map_or(String::new(), |&c| (c as char).to_string());
                    self.errors
                        .push(format!("`{shown}': invalid format character"));
                    self.stop = true;
                    break;
                }
                i += 1;
                self.time(&tfmt, &spec);
                continue;
            }
            // Length modifiers are accepted and ignored.
            while i < fmt.len() && b"hlLjzt".contains(&fmt[i]) {
                i += 1;
            }
            let Some(&conv) = fmt.get(i) else {
                self.errors
                    .push(format!("`{}': missing format character", s(&fmt[start..])));
                self.stop = true;
                break;
            };
            i += 1;
            self.convert(conv, &spec);
        }
    }

    /// `%(fmt)T`: an argument of seconds since the epoch (`-1` or none: now, `-2`:
    /// when the shell started) formatted with `strftime`.
    fn time(&mut self, tfmt: &[u8], spec: &Spec) {
        let t = match self.arg().map(<[u8]>::to_vec) {
            None => self.now,
            Some(a) if a.is_empty() => self.now,
            Some(a) => {
                let n = parse_num(&a, self.utf8);
                if !n.ok {
                    self.errors.push(format!("{}: invalid number", s(&a)));
                }
                let v = if n.neg { -(n.mag as i64) } else { n.mag as i64 };
                if v == -1 || v == -2 { self.now } else { v }
            }
        };
        let utc = super::strftime::Zone::utc();
        let zone = self.zone.as_ref().unwrap_or(&utc);
        let mut body = super::strftime::strftime(tfmt, zone, t);
        // bash formats into a buffer of 128 bytes; what does not fit is dropped.
        if body.len() >= 128 {
            body.clear();
        }
        if let Some(p) = spec.prec {
            body.truncate(p);
        }
        self.pad(&body, spec, false);
    }

    fn pad(&mut self, body: &[u8], spec: &Spec, numeric: bool) {
        let w = spec.width.unwrap_or(0);
        if body.len() >= w {
            self.out.extend_from_slice(body);
            return;
        }
        let fill = w - body.len();
        if spec.left {
            self.out.extend_from_slice(body);
            self.out.extend(std::iter::repeat_n(b' ', fill));
        } else if spec.zero && numeric {
            // Zeros go after the sign or 0x prefix.
            let split = if body.starts_with(b"0x") || body.starts_with(b"0X") {
                2
            } else {
                usize::from(matches!(body.first(), Some(b'-' | b'+' | b' ')))
            };
            self.out.extend_from_slice(&body[..split]);
            self.out.extend(std::iter::repeat_n(b'0', fill));
            self.out.extend_from_slice(&body[split..]);
        } else {
            self.out.extend(std::iter::repeat_n(b' ', fill));
            self.out.extend_from_slice(body);
        }
    }

    fn convert(&mut self, conv: u8, spec: &Spec) {
        match conv {
            b's' | b'b' | b'q' | b'Q' => {
                let mut a = self.arg().map(<[u8]>::to_vec).unwrap_or_default();
                // `%Q`: the precision cuts the argument, before it is quoted.
                if conv == b'Q'
                    && let Some(p) = spec.prec
                {
                    a.truncate(p);
                }
                let utf8 = self.utf8;
                let quote = |a: &[u8]| {
                    if spec.alt {
                        single_quote(a, utf8)
                    } else {
                        shell_quote(a, utf8)
                    }
                };
                let mut body = match conv {
                    b's' => a,
                    b'q' => quote(&a),
                    b'Q' => {
                        let q = quote(&a);
                        self.pad(&q, spec, false);
                        return;
                    }
                    _ => {
                        let d = decode(&a, Dialect::PercentB);
                        if d.stop {
                            self.stop = true;
                        }
                        d.bytes
                    }
                };
                if let Some(p) = spec.prec {
                    body.truncate(p);
                }
                self.pad(&body, spec, false);
            }
            b'c' => {
                let a = self.arg().map(<[u8]>::to_vec).unwrap_or_default();
                // An empty or missing argument is a NUL byte (bash).
                let body = vec![a.first().copied().unwrap_or(0)];
                self.pad(&body, spec, false);
            }
            b'd' | b'i' => {
                let v = self.num_arg();
                let mut digits = v.unsigned_abs().to_string();
                if let Some(p) = spec.prec {
                    while digits.len() < p {
                        digits.insert(0, '0');
                    }
                    if p == 0 && v == 0 {
                        digits.clear();
                    }
                }
                let sign = if v < 0 {
                    "-"
                } else if spec.plus {
                    "+"
                } else if spec.space {
                    " "
                } else {
                    ""
                };
                let body = format!("{sign}{digits}");
                let numeric = spec.prec.is_none();
                self.pad(body.as_bytes(), spec, numeric);
            }
            b'u' | b'o' | b'x' | b'X' => {
                let v = self.unsigned_arg();
                let mut digits = match conv {
                    b'u' => v.to_string(),
                    b'o' => format!("{v:o}"),
                    b'x' => format!("{v:x}"),
                    _ => format!("{v:X}"),
                };
                if let Some(p) = spec.prec {
                    while digits.len() < p {
                        digits.insert(0, '0');
                    }
                }
                if spec.alt && v != 0 {
                    match conv {
                        b'o' if !digits.starts_with('0') => digits.insert(0, '0'),
                        b'x' => digits.insert_str(0, "0x"),
                        b'X' => digits.insert_str(0, "0X"),
                        _ => {}
                    }
                }
                let numeric = spec.prec.is_none();
                self.pad(digits.as_bytes(), spec, numeric);
            }
            b'f' | b'F' | b'e' | b'E' | b'g' | b'G' => {
                let v = self.float_arg();
                let prec = spec.prec.unwrap_or(6);
                let mut body = match conv {
                    // `#` keeps the point even without decimals.
                    b'f' | b'F' if spec.alt && prec == 0 && v.is_finite() => format!("{v:.0}."),
                    b'f' | b'F' => format!("{v:.prec$}"),
                    b'e' | b'E' if spec.alt && prec == 0 && v.is_finite() => {
                        fmt_e(v, 0).replacen('e', ".e", 1)
                    }
                    b'e' | b'E' => fmt_e(v, prec),
                    _ => fmt_g(v, prec, spec.alt),
                };
                if conv.is_ascii_uppercase() {
                    body = body.to_uppercase();
                }
                if v >= 0.0 && !body.starts_with('-') {
                    if spec.plus {
                        body.insert(0, '+');
                    } else if spec.space {
                        body.insert(0, ' ');
                    }
                }
                self.pad(body.as_bytes(), spec, true);
            }
            other => {
                self.errors
                    .push(format!("`{}': invalid format character", other as char));
                self.stop = true;
            }
        }
    }
}

/// C's `%e`: `d.dddddde+XX`.
fn fmt_e(v: f64, prec: usize) -> String {
    if !v.is_finite() {
        return if v.is_nan() {
            "nan".into()
        } else if v > 0.0 {
            "inf".into()
        } else {
            "-inf".into()
        };
    }
    let s = format!("{v:.prec$e}");
    let (mant, exp) = s.split_once('e').unwrap_or((&s, "0"));
    let exp: i32 = exp.parse().unwrap_or(0);
    let sign = if exp < 0 { '-' } else { '+' };
    format!("{mant}e{sign}{:02}", exp.abs())
}

/// C's `%g`: the shorter of `%e` and `%f` with `prec` significant digits, trailing
/// zeros removed unless `#`.
fn fmt_g(v: f64, prec: usize, alt: bool) -> String {
    if !v.is_finite() {
        return fmt_e(v, 0);
    }
    let p = if prec == 0 { 1 } else { prec };
    let exp = if v == 0.0 {
        0
    } else {
        let e = format!("{v:.*e}", p - 1);
        e.split_once('e')
            .and_then(|(_, x)| x.parse::<i32>().ok())
            .unwrap_or(0)
    };
    let mut out = if exp < -4 || exp >= p as i32 {
        fmt_e(v, p - 1)
    } else {
        let decimals = (p as i32 - 1 - exp).max(0) as usize;
        format!("{v:.decimals$}")
    };
    if !alt {
        let (num, suffix) = match out.find('e') {
            Some(i) => (out[..i].to_string(), out[i..].to_string()),
            None => (out.clone(), String::new()),
        };
        let num = if num.contains('.') {
            num.trim_end_matches('0').trim_end_matches('.').to_string()
        } else {
            num
        };
        out = format!("{num}{suffix}");
    }
    out
}

struct Num {
    neg: bool,
    /// The magnitude, if it fits in 64 bits.
    mag: u128,
    /// More than 64 bits.
    overflow: bool,
    /// The whole argument was a number.
    ok: bool,
}

/// A `'c` or `"c` argument (the quote first, no blanks before it): the code of the
/// character after the quote, as bash's `asciicode` does with `mbtowc`. In a UTF-8
/// locale a valid multibyte character gives its code point; otherwise (or in the C
/// locale) the first byte. Nothing after the quote is 0.
fn char_code(a: &[u8], utf8: bool) -> Option<u32> {
    let rest = a.strip_prefix(b"'").or_else(|| a.strip_prefix(b"\""))?;
    let Some(&first) = rest.first() else {
        return Some(0);
    };
    if utf8 && first >= 0x80 {
        let n = match first {
            0xC2..=0xDF => 2,
            0xE0..=0xEF => 3,
            0xF0..=0xF4 => 4,
            _ => 0,
        };
        if let Some(c) = rest
            .get(..n)
            .and_then(|b| std::str::from_utf8(b).ok())
            .and_then(|t| t.chars().next())
        {
            return Some(u32::from(c));
        }
    }
    Some(u32::from(first))
}

/// Parses an integer argument like `strtoimax` with base 0, plus `'c` for a char code.
fn parse_num(a: &[u8], utf8: bool) -> Num {
    if let Some(v) = char_code(a, utf8) {
        return Num {
            neg: false,
            mag: u128::from(v),
            overflow: false,
            ok: true,
        };
    }
    let t = s(a);
    let t = t.trim_start();
    let (neg, body) = match t.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, t.strip_prefix('+').unwrap_or(t)),
    };
    let (radix, digits) =
        if let Some(h) = body.strip_prefix("0x").or_else(|| body.strip_prefix("0X")) {
            (16, h)
        } else if body.len() > 1 && body.starts_with('0') {
            (8, &body[1..])
        } else {
            (10, body)
        };
    let end = digits
        .find(|c: char| !c.is_digit(radix))
        .unwrap_or(digits.len());
    let mut mag = 0u128;
    let mut overflow = false;
    for c in digits[..end].chars() {
        mag = mag * u128::from(radix) + u128::from(c.to_digit(radix).unwrap_or(0));
        if mag > u128::from(u64::MAX) {
            overflow = true;
            mag = u128::from(u64::MAX);
        }
    }
    let ok = end == digits.len() && (end > 0 || radix == 8) && !t.is_empty();
    Num {
        neg,
        mag,
        overflow,
        ok,
    }
}

/// The value of the longest prefix of `t` that C's `strtod` converts: a decimal or
/// hexadecimal number with its exponent, or `inf`, `infinity` or `nan`. None when it
/// converts nothing (`-n`, `e3`). The second value is how many bytes it converted.
fn float_prefix(t: &[u8]) -> Option<(f64, usize)> {
    let (neg, body) = match t.first() {
        Some(b'-') => (true, &t[1..]),
        Some(b'+') => (false, &t[1..]),
        _ => (false, t),
    };
    let sign = t.len() - body.len();
    let lower = body.to_ascii_lowercase();
    let (v, used) = if lower.starts_with(b"infinity") {
        (f64::INFINITY, 8)
    } else if lower.starts_with(b"inf") {
        (f64::INFINITY, 3)
    } else if lower.starts_with(b"nan") {
        (f64::NAN, 3)
    } else if let Some((v, n)) = lower.strip_prefix(b"0x").and_then(hex_float) {
        (v, n + 2)
    } else {
        let digits = |i: &mut usize| {
            let start = *i;
            while body.get(*i).is_some_and(u8::is_ascii_digit) {
                *i += 1;
            }
            *i - start
        };
        let mut i = 0;
        let mut n = digits(&mut i);
        if body.get(i) == Some(&b'.') {
            i += 1;
            n += digits(&mut i);
        }
        if n == 0 {
            return None;
        }
        let mantissa = i;
        if matches!(body.get(i), Some(b'e' | b'E')) {
            let mut j = i + 1;
            if matches!(body.get(j), Some(b'+' | b'-')) {
                j += 1;
            }
            if digits(&mut j) > 0 {
                i = j;
            }
        }
        match std::str::from_utf8(&body[..i]).ok()?.parse::<f64>() {
            Ok(v) => (v, i),
            Err(_) => (
                std::str::from_utf8(&body[..mantissa]).ok()?.parse().ok()?,
                mantissa,
            ),
        }
    };
    Some((if neg { -v } else { v }, sign + used))
}

/// A hexadecimal float's value after its `0x` (digits, a fraction and a `p` exponent),
/// and how many bytes it takes.
fn hex_float(b: &[u8]) -> Option<(f64, usize)> {
    let mut i = 0;
    let mut v = 0f64;
    let mut n = 0;
    while let Some(d) = b.get(i).and_then(|c| (*c as char).to_digit(16)) {
        v = v * 16.0 + f64::from(d);
        i += 1;
        n += 1;
    }
    if b.get(i) == Some(&b'.') {
        i += 1;
        let mut scale = 1.0 / 16.0;
        while let Some(d) = b.get(i).and_then(|c| (*c as char).to_digit(16)) {
            v += f64::from(d) * scale;
            scale /= 16.0;
            i += 1;
            n += 1;
        }
    }
    if n == 0 {
        return None;
    }
    if matches!(b.get(i), Some(b'p' | b'P')) {
        let mut j = i + 1;
        let neg = match b.get(j) {
            Some(b'-') => {
                j += 1;
                true
            }
            Some(b'+') => {
                j += 1;
                false
            }
            _ => false,
        };
        let start = j;
        let mut e: i32 = 0;
        while let Some(d) = b.get(j).filter(|c| c.is_ascii_digit()) {
            e = e.saturating_mul(10).saturating_add(i32::from(d - b'0'));
            j += 1;
        }
        if j > start {
            v *= 2f64.powi(if neg { -e } else { e });
            i = j;
        }
    }
    Some((v, i))
}

#[cfg(test)]
mod tests {
    use super::{char_code, parse_num, shell_quote};

    #[test]
    fn char_constant_follows_the_locale() {
        // `printf %d "'é"`: the code point in UTF-8, the first byte in C.
        assert_eq!(char_code("'été'-x".as_bytes(), true), Some(0xE9));
        assert_eq!(char_code("'été'-x".as_bytes(), false), Some(0xC3));
        // An invalid sequence is its first byte even in UTF-8.
        assert_eq!(char_code(b"'\xff", true), Some(0xFF));
        assert_eq!(char_code(b"\"", true), Some(0));
        // Blanks before the quote make it an ordinary (invalid) number.
        assert_eq!(char_code(b" 'a", true), None);
        assert_eq!(parse_num("'é".as_bytes(), false).mag, 0xC3);
        assert!(!parse_num(b" 'a", false).ok);
    }

    #[test]
    fn percent_q_leaves_a_leading_equals_alone() {
        assert_eq!(shell_quote(b"=v", true), b"=v");
        assert_eq!(shell_quote(b"=~", true), b"=\\~");
        assert_eq!(shell_quote(b"~=", true), b"\\~=");
    }
}
