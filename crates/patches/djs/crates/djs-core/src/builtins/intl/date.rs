use super::data::{DATE_PATTERNS, LOCALE_DATES, PATTERNS, STYLE_PATTERNS, TIME_PATTERNS, ZONE_ALIASES, ZONE_NAMES, ZONES};
use super::{Parts, Spec, range, type_err};
use crate::interp::conv::Fault;
use alloc::format;
use alloc::string::{String, ToString};

const WIDTHS: [&str; 4] = ["", "long", "short", "narrow"];
const NUMS: [&str; 3] = ["", "numeric", "2-digit"];
const MONTHS: [&str; 6] = ["", "numeric", "2-digit", "long", "short", "narrow"];
const CYCLES: [&str; 4] = ["h12", "h23", "h11", "h24"];
const STYLES: [&str; 4] = ["full", "long", "medium", "short"];
const TZ_NAMES: [&str; 6] = ["short", "long", "shortOffset", "longOffset", "shortGeneric", "longGeneric"];
const TZ_LETTERS: [&str; 6] = ["z", "zzzz", "O", "OOOO", "v", "vvvv"];
const DAY_NAMES: [&str; 7] = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];
const MONTH_NAMES: [&str; 12] = [
    "January", "February", "March", "April", "May", "June", "July", "August", "September", "October", "November",
    "December",
];

fn pos(values: &[&str], v: Option<&str>) -> usize {
    values.iter().position(|x| Some(*x) == v).unwrap_or(0)
}

/// The zone's canonical name, from a case-insensitive IANA name, alias or offset.
fn canonical_zone(name: &str) -> Option<String> {
    let b = name.as_bytes();
    if matches!(b.first(), Some(b'+' | b'-')) {
        let digits: String = name[1..].chars().filter(|c| *c != ':').collect();
        let ok = name[1..].bytes().all(|c| c.is_ascii_digit() || c == b':');
        let (h, m) = match digits.len() {
            2 => (&digits[..2], "00"),
            4 => (&digits[..2], &digits[2..]),
            _ => return None,
        };
        let colon_ok = !name.contains(':') || (name.len() == 6 && b[3] == b':');
        if !ok || !colon_ok || h > "23" || m > "59" {
            return None;
        }
        return Some(format!("{}{h}:{m}", &name[..1]));
    }
    if let Some(z) = ZONES.iter().find(|z| z.0.eq_ignore_ascii_case(name)) {
        return Some(z.0.to_string());
    }
    ZONE_ALIASES
        .iter()
        .find(|a| a.0.eq_ignore_ascii_case(name))
        .map(|a| a.1.to_string())
        .or_else(|| {
            let upper = name.to_ascii_uppercase();
            let etc = upper.strip_prefix("ETC/").unwrap_or(&upper);
            matches!(etc, "UTC" | "GMT" | "UCT" | "ZULU" | "UNIVERSAL" | "GREENWICH" | "GMT0" | "GMT+0" | "GMT-0")
                .then(|| "UTC".to_string())
        })
}

pub(super) fn resolve(locale: &str, raw: &Spec) -> Result<Spec, Fault> {
    let n = "Intl.DateTimeFormat";
    let hour12 = raw.get("hour12").map(|v| v == "true");
    let hc_opt = raw.choose_opt("hourCycle", &CYCLES, n)?;
    let zone = match raw.get("timeZone") {
        Some(z) => match canonical_zone(z) {
            Some(c) => c,
            None => return range(&format!("Invalid time zone specified: {z}")),
        },
        None => {
            let off = super::parse_f64(raw.get("@offset").unwrap_or("0")) as i64;
            if off == 0 { "UTC".to_string() } else { offset_name(off, true, true) }
        }
    };
    let mut fields = [0usize; 10];
    fields[0] = pos(&WIDTHS, raw.choose_opt("weekday", &WIDTHS[1..], n)?);
    fields[1] = pos(&WIDTHS, raw.choose_opt("era", &WIDTHS[1..], n)?);
    fields[2] = pos(&NUMS, raw.choose_opt("year", &NUMS[1..], n)?);
    fields[3] = pos(&MONTHS, raw.choose_opt("month", &MONTHS[1..], n)?);
    fields[4] = pos(&NUMS, raw.choose_opt("day", &NUMS[1..], n)?);
    fields[5] = pos(&WIDTHS, raw.choose_opt("dayPeriod", &["narrow", "short", "long"], n)?.map(|p| match p {
        "narrow" => "narrow",
        "short" => "short",
        _ => "long",
    }));
    fields[6] = pos(&NUMS, raw.choose_opt("hour", &NUMS[1..], n)?);
    fields[7] = pos(&NUMS, raw.choose_opt("minute", &NUMS[1..], n)?);
    fields[8] = pos(&NUMS, raw.choose_opt("second", &NUMS[1..], n)?);
    let fsd = match raw.get("fractionalSecondDigits") {
        None => 0,
        Some(v) => {
            let x = super::parse_f64(v);
            if !(1.0..=3.0).contains(&x) {
                return range("fractionalSecondDigits value is out of range.");
            }
            x as usize
        }
    };
    let tzn = raw.choose_opt("timeZoneName", &TZ_NAMES, n)?;
    raw.choose("formatMatcher", &["basic", "best fit"], "best fit", n)?;
    let ds = raw.choose_opt("dateStyle", &STYLES, n)?;
    let ts = raw.choose_opt("timeStyle", &STYLES, n)?;
    let explicit = fields.iter().any(|f| *f != 0) || fsd != 0 || tzn.is_some();
    if (ds.is_some() || ts.is_some()) && explicit {
        return type_err("Invalid option : option");
    }
    let need = raw.get("@required").unwrap_or("any");
    let defaults = raw.get("@defaults").unwrap_or("date");
    if ds.is_none() && ts.is_none() {
        let has_date = fields[0] != 0 || fields[2] != 0 || fields[3] != 0 || fields[4] != 0;
        let has_time = fields[5] != 0 || fields[6] != 0 || fields[7] != 0 || fields[8] != 0 || fsd != 0;
        let need_defaults = match need {
            "date" => !has_date,
            "time" => !has_time,
            _ => !has_date && !has_time,
        };
        if need_defaults && defaults != "time" {
            fields[2] = 1;
            fields[3] = 1;
            fields[4] = 1;
        }
        if need_defaults && defaults != "date" {
            fields[6] = 1;
            fields[7] = 1;
            fields[8] = 1;
        }
    } else if need == "date" && ds.is_none() {
        return type_err("Invalid option : dateStyle");
    } else if need == "time" && ts.is_none() {
        return type_err("Invalid option : timeStyle");
    }
    let cycle = match (hour12, hc_opt) {
        (Some(true), _) => 0,
        (Some(false), _) => 1,
        (None, Some(c)) => pos(&CYCLES, Some(c)),
        (None, None) => 0,
    };
    let local = LOCALE_DATES
        .iter()
        .find(|l| l.0 == locale)
        .or_else(|| LOCALE_DATES.iter().find(|l| locale.split('-').next() == Some(l.0)));
    let pattern = match (ds, ts) {
        (None, None) => match local.and_then(|l| local_pattern(l, &fields, fsd, hour12.is_some() || hc_opt.is_some() || tzn.is_some())) {
            Some(p) => p,
            None => compose(&fields, fsd, cycle, tzn),
        },
        (d, t) => {
            let dp = d.map(|s| PATTERNS[usize::from(STYLE_PATTERNS[pos(&STYLES, Some(s))])]);
            let tp = t.map(|s| PATTERNS[usize::from(STYLE_PATTERNS[4 + cycle * 4 + pos(&STYLES, Some(s))])]);
            match (dp, tp) {
                (Some(d), Some(t)) => {
                    let glue = if matches!(ds, Some("full" | "long")) { "' at '" } else { ", " };
                    format!("{d}{glue}{t}")
                }
                (Some(d), None) => d.to_string(),
                (None, Some(t)) => t.to_string(),
                (None, None) => String::new(),
            }
        }
    };
    let mut out = Spec::default();
    out.put("locale", locale);
    out.put("calendar", "gregory");
    out.put("numberingSystem", "latn");
    out.put("timeZone", &zone);
    let has_hour = pattern.contains(['h', 'H', 'k', 'K']);
    if has_hour {
        let hc = if pattern.contains('h') { "h12" } else if pattern.contains('K') { "h11" } else if pattern.contains('k') { "h24" } else { "h23" };
        out.put("hourCycle", hc);
        out.put("hour12", if hc == "h12" || hc == "h11" { "!t" } else { "!f" });
    }
    if ds.is_none() && ts.is_none() {
        for (name, letter) in [("weekday", 'E'), ("era", 'G'), ("year", 'y'), ("month", 'M'), ("day", 'd'), ("dayPeriod", 'B'), ("hour", 'j'), ("minute", 'm'), ("second", 's'), ("fractionalSecondDigits", 'S'), ("timeZoneName", 'z')] {
            let count = field_width(&pattern, letter);
            if count == 0 {
                continue;
            }
            let v: &str = match letter {
                'E' | 'G' => match count { 4 => "long", 5 => "narrow", _ => "short" },
                'B' => match fields[5] { 3 => "narrow", 2 => "short", _ => "long" },
                'M' => match count { 1 => "numeric", 2 => "2-digit", 3 => "short", 4 => "long", _ => "narrow" },
                'S' => {
                    out.num(name, count as u32);
                    continue;
                }
                'z' => tzn.unwrap_or("short"),
                _ => if count == 2 { "2-digit" } else { "numeric" },
            };
            out.put(name, v);
        }
    } else {
        if let Some(d) = ds {
            out.put("dateStyle", d);
        }
        if let Some(t) = ts {
            out.put("timeStyle", t);
        }
    }
    if let Some(l) = local {
        out.put("@ampm", &format!("{},{}", l.5[0], l.5[1]));
    }
    out.put("@pattern", &pattern);
    Ok(out)
}

/// A locale's own pattern for numeric dates and times (the few locales with data), else None.
fn local_pattern(l: &(&str, &str, &str, &str, &str, [&str; 2]), f: &[usize; 10], fsd: usize, custom: bool) -> Option<String> {
    if custom || fsd != 0 || f[0] != 0 || f[1] != 0 || f[5] != 0 {
        return None;
    }
    let date = match (f[2], f[3], f[4]) {
        (0, 0, 0) => None,
        (1, 1, 1) => Some(l.1),
        _ => return None,
    };
    let time = match (f[6], f[7], f[8]) {
        (0, 0, 0) => None,
        (1, 1, 1) => Some(l.2),
        (1, 1, 0) => Some(l.4),
        _ => return None,
    };
    match (date, time) {
        (Some(d), Some(t)) => Some(format!("{d}{}{t}", l.3)),
        (Some(d), None) => Some(d.to_string()),
        (None, Some(t)) => Some(t.to_string()),
        (None, None) => None,
    }
}

/// The longest run of a field's letter outside quotes (\`j\`: any hour letter).
fn field_width(p: &str, letter: char) -> usize {
    let mut best = 0;
    let mut quoted = false;
    let mut run = 0;
    let mut prev = '\0';
    for c in p.chars() {
        if c == '\'' {
            quoted = !quoted;
            run = 0;
            prev = c;
            continue;
        }
        let hit = !quoted
            && (c == letter
                || (letter == 'j' && matches!(c, 'h' | 'H' | 'k' | 'K'))
                || (letter == 'z' && matches!(c, 'z' | 'O' | 'v')));
        run = if hit && (run == 0 || c == prev) { run + 1 } else if hit { 1 } else { 0 };
        best = best.max(run);
        prev = c;
    }
    best
}

fn insert_zone(t: &str, z: &str) -> String {
    match t.find("' (").or_else(|| t.find("' \u{251c}")) {
        Some(i) => format!("{} {z}{}", &t[..i], &t[i..]),
        None => format!("{t} {z}"),
    }
}

fn compose(f: &[usize; 10], fsd: usize, cycle: usize, tzn: Option<&str>) -> String {
    let dk = (((f[0] * 4 + f[1]) * 3 + f[2]) * 6 + f[3]) * 3 + f[4];
    let tk = ((((f[6] * 3 + f[7]) * 3 + f[8]) * 4 + cycle) * 4 + f[5]) * 4 + fsd;
    let lookup = |i: u16| (i != u16::MAX).then(|| PATTERNS[usize::from(i)]);
    let mut d = lookup(DATE_PATTERNS[dk]).map(String::from);
    let mut t = lookup(TIME_PATTERNS[tk]).map(String::from);
    let only_era = f[1] != 0 && f[0] == 0 && f[2] == 0 && f[3] == 0 && f[4] == 0;
    if let Some(time) = &mut t {
        if d.is_some() && f[6] == 0 {
            if let Some(i) = time.find("' \u{251c}AM/PM: '") {
                let end = time[i..].find('\u{2524}').map_or(time.len(), |j| i + j + '\u{2524}'.len_utf8());
                time.replace_range(i..end, "");
            }
        }
        if tzn.is_some() && let Some(i) = time.find(" B") {
            time.replace_range(i..i + 1, "\u{202f}");
        }
    }
    if let Some(z) = tzn {
        let letters = TZ_LETTERS[pos(&TZ_NAMES, Some(z))];
        t = Some(match &t {
            Some(time) => insert_zone(time, letters),
            None => letters.to_string(),
        });
    }
    if only_era && t.is_some() {
        d = Some(["", "GGGG", "G", "GGGGG"][f[1]].to_string());
    }
    match (d, t) {
        (Some(d), Some(t)) => {
            let weekday_only = d.bytes().all(|b| b == b'E');
            let h12 = cycle == 0 || cycle == 2;
            if weekday_only && f[6] != 0 && fsd == 0 && tzn.is_none() && (h12 || f[7] != 0) {
                format!("{d} {t}")
            } else if f[3] == 3 {
                format!("{d}' at '{t}")
            } else {
                format!("{d}, {t}")
            }
        }
        (Some(d), None) => d,
        (None, Some(t)) => t,
        (None, None) => String::new(),
    }
}

// ---- time zones ----

struct Rule {
    std: i64,
    dst: Option<(i64, Trans, Trans)>,
}

#[derive(Clone, Copy)]
enum Day {
    Month(u32, u32, u32),
    Julian(u32),
    Zero(u32),
}

#[derive(Clone, Copy)]
struct Trans {
    day: Day,
    secs: i64,
}

fn parse_offset(s: &[u8], i: &mut usize) -> Option<i64> {
    let mut sign = 1;
    if let Some(c) = s.get(*i)
        && (*c == b'+' || *c == b'-')
    {
        if *c == b'-' {
            sign = -1;
        }
        *i += 1;
    }
    let mut parts = [0i64; 3];
    for (k, part) in parts.iter_mut().enumerate() {
        if k > 0 {
            if s.get(*i) != Some(&b':') {
                break;
            }
            *i += 1;
        }
        let start = *i;
        while s.get(*i).is_some_and(u8::is_ascii_digit) {
            *part = *part * 10 + i64::from(s[*i] - b'0');
            *i += 1;
        }
        if *i == start {
            return None;
        }
    }
    Some(sign * (parts[0] * 3600 + parts[1] * 60 + parts[2]))
}

fn skip_name(s: &[u8], i: &mut usize) {
    if s.get(*i) == Some(&b'<') {
        while *i < s.len() && s[*i] != b'>' {
            *i += 1;
        }
        *i += 1;
    } else {
        while s.get(*i).is_some_and(u8::is_ascii_alphabetic) {
            *i += 1;
        }
    }
}

fn parse_trans(s: &[u8], i: &mut usize) -> Option<Trans> {
    let num = |i: &mut usize| {
        let mut v = 0u32;
        while s.get(*i).is_some_and(u8::is_ascii_digit) {
            v = v * 10 + u32::from(s[*i] - b'0');
            *i += 1;
        }
        v
    };
    let day = match s.get(*i) {
        Some(b'M') => {
            *i += 1;
            let m = num(i);
            *i += 1;
            let w = num(i);
            *i += 1;
            Day::Month(m, w, num(i))
        }
        Some(b'J') => {
            *i += 1;
            Day::Julian(num(i))
        }
        _ => Day::Zero(num(i)),
    };
    let secs = if s.get(*i) == Some(&b'/') {
        *i += 1;
        parse_offset(s, i)?
    } else {
        7200
    };
    Some(Trans { day, secs })
}

fn rule(posix: &str) -> Rule {
    let s = posix.as_bytes();
    let mut i = 0;
    skip_name(s, &mut i);
    let std = -parse_offset(s, &mut i).unwrap_or(0);
    if i >= s.len() {
        return Rule { std, dst: None };
    }
    skip_name(s, &mut i);
    let dst = if s.get(i) == Some(&b',') { std + 3600 } else { -parse_offset(s, &mut i).unwrap_or(-(std + 3600)) };
    i += 1;
    let (Some(a), true) = (parse_trans(s, &mut i), s.get(i) == Some(&b',')) else {
        return Rule { std, dst: None };
    };
    i += 1;
    match parse_trans(s, &mut i) {
        Some(b) => Rule { std, dst: Some((dst, a, b)) },
        None => Rule { std, dst: None },
    }
}

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = i64::from((m + 9) % 12);
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { yoe + era * 400 + 1 } else { yoe + era * 400 }, m, d)
}

fn leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

/// Seconds since the epoch, in local wall time, of the transition in year y.
fn trans_local(t: Trans, y: i64) -> i64 {
    let day = match t.day {
        Day::Month(m, w, wd) => {
            let first = days_from_civil(y, m, 1);
            let first_wd = (first + 4).rem_euclid(7) as u32;
            let mut d = (wd + 7 - first_wd) % 7 + 1 + (w - 1) * 7;
            let len = [31, if leap(y) { 29 } else { 28 }, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31][(m - 1) as usize];
            while d > len {
                d -= 7;
            }
            first + i64::from(d) - 1
        }
        Day::Julian(n) => {
            let n = i64::from(n);
            days_from_civil(y, 1, 1) + n - 1 + i64::from(leap(y) && n >= 60)
        }
        Day::Zero(n) => days_from_civil(y, 1, 1) + i64::from(n),
    };
    day * 86_400 + t.secs
}

/// (offset in seconds, in daylight time) at UTC seconds \`t\`.
fn zone_offset(zone: &str, t: i64) -> (i64, bool, Option<&'static (&'static str, &'static str, [u16; 6])>) {
    if let Some(rest) = zone.strip_prefix(['+', '-']) {
        let h: i64 = rest[..2].parse().unwrap_or(0);
        let m: i64 = rest[3..].parse().unwrap_or(0);
        let off = (h * 3600 + m * 60) * if zone.starts_with('-') { -1 } else { 1 };
        return (off, false, None);
    }
    let Some(z) = ZONES.iter().find(|z| z.0 == zone) else { return (0, false, None) };
    let r = rule(z.1);
    let Some((dst, a, b)) = r.dst else { return (r.std, false, Some(z)) };
    let (y, _, _) = civil((t + r.std).div_euclid(86_400));
    let start = trans_local(a, y) - r.std;
    let end = trans_local(b, y) - dst;
    let in_dst = if start < end { t >= start && t < end } else { !(t >= end && t < start) };
    (if in_dst { dst } else { r.std }, in_dst, Some(z))
}

fn offset_name(minutes: i64, long: bool, bare: bool) -> String {
    let sign = if minutes < 0 { '-' } else { '+' };
    let (h, m) = (minutes.abs() / 60, minutes.abs() % 60);
    if bare {
        return format!("{sign}{h:02}:{m:02}");
    }
    if minutes == 0 {
        return "GMT".to_string();
    }
    if long {
        format!("GMT{sign}{h:02}:{m:02}")
    } else if m == 0 {
        format!("GMT{sign}{h}")
    } else {
        format!("GMT{sign}{h}:{m:02}")
    }
}

fn zone_name(kind: usize, off: i64, dst: bool, entry: Option<&(&str, &str, [u16; 6])>) -> String {
    let minutes = off / 60;
    match (kind, entry) {
        (2, _) | (3, _) => offset_name(minutes, kind == 3, false),
        (_, None) => offset_name(minutes, kind == 1 || kind == 5, false),
        (_, Some(z)) => {
            let i = match kind {
                0 => usize::from(dst),
                1 => 2 + usize::from(dst),
                4 => 4,
                _ => 5,
            };
            ZONE_NAMES[usize::from(z.2[i])].to_string()
        }
    }
}

// ---- formatting ----

pub(super) fn format(spec: &Spec, t: f64) -> Result<Parts, Fault> {
    if !t.is_finite() || t.abs() > 8.64e15 {
        return range("Invalid time value");
    }
    let ms = t as i64;
    let zone = spec.get("timeZone").unwrap_or("UTC");
    let secs = ms.div_euclid(1000);
    let (off, dst, entry) = zone_offset(zone, secs);
    let local = ms + off * 1000;
    let days = local.div_euclid(86_400_000);
    let in_day = local.rem_euclid(86_400_000);
    let (y, mo, d) = civil(days);
    let wd = (days + 4).rem_euclid(7) as usize;
    let (h, mi, s, frac) = (in_day / 3_600_000, in_day / 60_000 % 60, in_day / 1000 % 60, in_day % 1000);
    let pattern = spec.get("@pattern").unwrap_or("");
    let mut out = Parts::default();
    let chars: alloc::vec::Vec<char> = pattern.chars().collect();
    let mut i = 0;
    let num = |v: i64, width: usize| if width >= 2 { format!("{:02}", v % 100) } else { v.to_string() };
    while i < chars.len() {
        let c = chars[i];
        if c == '\'' {
            let mut j = i + 1;
            let mut lit = String::new();
            while j < chars.len() {
                if chars[j] == '\'' {
                    if chars.get(j + 1) == Some(&'\'') {
                        lit.push('\'');
                        j += 2;
                        continue;
                    }
                    break;
                }
                lit.push(chars[j]);
                j += 1;
            }
            if lit.is_empty() && j == i + 1 {
                lit.push('\'');
            }
            out.push("literal", &lit);
            i = j + 1;
            continue;
        }
        if !c.is_ascii_alphabetic() {
            let mut lit = String::new();
            lit.push(c);
            out.push("literal", &lit);
            i += 1;
            continue;
        }
        let mut n = 1;
        while chars.get(i + n) == Some(&c) {
            n += 1;
        }
        i += n;
        let era_year = if y <= 0 { 1 - y } else { y };
        match c {
            'E' => {
                let name = DAY_NAMES[wd];
                let v = match n {
                    4 => name.to_string(),
                    5 => name[..1].to_string(),
                    _ => name[..3].to_string(),
                };
                out.push("weekday", &v);
            }
            'G' => {
                let v = match (n, y > 0) {
                    (4, true) => "Anno Domini",
                    (4, false) => "Before Christ",
                    (5, true) => "A",
                    (5, false) => "B",
                    (_, true) => "AD",
                    _ => "BC",
                };
                out.push("era", v);
            }
            'y' => out.push("year", &if n == 2 { format!("{:02}", era_year % 100) } else { era_year.to_string() }),
            'M' | 'L' => {
                let name = MONTH_NAMES[(mo - 1) as usize];
                let v = match n {
                    1 | 2 => num(i64::from(mo), n),
                    3 => name[..3].to_string(),
                    4 => name.to_string(),
                    _ => name[..1].to_string(),
                };
                out.push("month", &v);
            }
            'd' => out.push("day", &num(i64::from(d), n)),
            'h' => out.push("hour", &num(if h % 12 == 0 { 12 } else { h % 12 }, n)),
            'K' => out.push("hour", &num(h % 12, n)),
            'H' => out.push("hour", &num(h, n)),
            'k' => out.push("hour", &num(if h == 0 { 24 } else { h }, n)),
            'm' => out.push("minute", &num(mi, n)),
            's' => out.push("second", &num(s, n)),
            'S' => {
                let digits = format!("{frac:03}");
                out.push("fractionalSecond", &digits[..n.min(3)]);
            }
            'a' => {
                let (am, pm) = spec.get("@ampm").and_then(|s| s.split_once(',')).unwrap_or(("AM", "PM"));
                out.push("dayPeriod", if h < 12 { am } else { pm });
            }
            'B' => {
                let v = match h {
                    12 if mi == 0 && s == 0 => if n == 5 { "n" } else { "noon" },
                    0..=11 => "in the morning",
                    12..=17 => "in the afternoon",
                    18..=20 => "in the evening",
                    _ => "at night",
                };
                out.push("dayPeriod", v);
            }
            'z' | 'O' | 'v' => {
                let kind = match (c, n) {
                    ('z', 4) => 1,
                    ('z', _) => 0,
                    ('O', 4) => 3,
                    ('O', _) => 2,
                    ('v', 4) => 5,
                    _ => 4,
                };
                out.push("timeZoneName", &zone_name(kind, off, dst, entry));
            }
            _ => {}
        }
    }
    Ok(out)
}
