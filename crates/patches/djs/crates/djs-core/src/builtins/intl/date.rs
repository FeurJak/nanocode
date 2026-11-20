use super::data::{DATE_PATTERNS, LOCALE_DATES, LocaleDates, PATTERNS, STYLE_PATTERNS, TIME_PATTERNS, ZONE_ALIASES, ZONE_NAMES, ZONES};
use super::{Parts, Spec, range, type_err};
use crate::interp::conv::Fault;
use alloc::format;
use alloc::string::{String, ToString};
use locales::{DATE_LOCALES, DTF_PATTERNS_ENDS, DTF_PATTERNS_TEXT, DTF_ROWS, DTF_STRINGS_ENDS, DTF_STRINGS_TEXT, EN_GENERIC_ALT};

mod locales;

/// A locale's date and time data, generated from Node's ICU by intl-dtf/emit.js.
/// Name arrays: months and days are [format long, short, narrow, standalone long, short, narrow];
/// eras and periods are [long, short, narrow]; periods hold 24 hours then exact midnight and noon.
/// Pattern tables index DTF_ROWS (rows of 16 pattern indices, u16::MAX: none) by date key
/// and by time key with seven hour-cycle options (none, hour12 true/false, h12, h23, h11, h24).
pub(super) struct DateLocale {
    tags: &'static [&'static str],
    months: [[&'static str; 12]; 6],
    days: [[&'static str; 7]; 6],
    eras: [[&'static str; 2]; 3],
    ampm: [&'static str; 2],
    periods: [[&'static str; 26]; 3],
    date: &'static [u16; 54],
    datez: &'static [u16; 54],
    time: &'static [u16; 189],
    timez: &'static [u16; 189],
    timez_ex: [&'static [(u16, u16)]; 6],
    weekday_ex: &'static [(u16, u16)],
    style_date: [u16; 4],
    style_time: [[u16; 4]; 7],
    style_glue: [&'static str; 4],
    glue: [&'static str; 4],
    zones: &'static [u16],
    gmt: [&'static str; 2],
}

pub(super) const LANGS: [&str; 17] = ["en", "de", "fr", "es", "it", "pt", "nl", "ja", "zh", "ko", "ru", "pl", "tr", "ar", "hi", "id", "vi"];

fn date_locale(locale: &str) -> Option<usize> {
    let find = |t: &str| DATE_LOCALES.iter().position(|l| l.tags.contains(&t));
    if let Some(i) = find(locale) {
        return Some(i);
    }
    let mut sub = locale.split('-');
    let lang = sub.next().unwrap_or("");
    if lang == "zh" && sub.any(|s| matches!(s, "Hant" | "TW" | "HK" | "MO")) {
        return find("zh-Hant");
    }
    find(lang)
}

const ARAB_DIGIT_REGIONS: [&str; 21] = ["BH", "DJ", "EG", "ER", "IL", "IQ", "JO", "KM", "KW", "LB", "MR", "OM", "PS", "QA", "SA", "SD", "SO", "SS", "SY", "TD", "YE"];

fn numbering_system(locale: &str) -> &'static str {
    let mut sub = locale.split('-');
    if sub.next() == Some("ar") && sub.any(|s| ARAB_DIGIT_REGIONS.contains(&s)) {
        "arab"
    } else {
        "latn"
    }
}

fn dtf_str(text: &'static str, ends: &[u32], i: u16) -> &'static str {
    let i = usize::from(i);
    let start = if i == 0 { 0 } else { ends[i - 1] as usize };
    &text[start..ends[i] as usize]
}

fn cell(rows: &[u16], k: usize) -> u16 {
    DTF_ROWS[usize::from(rows[k >> 4])][k & 15]
}

fn join(glue: &str, d: &str, t: &str) -> String {
    if glue.contains('{') {
        glue.replacen("{1}", d, 1).replacen("{0}", t, 1)
    } else {
        format!("{d}{glue}{t}")
    }
}

fn dtf_pattern(i: u16) -> Option<&'static str> {
    (i != u16::MAX).then(|| dtf_str(DTF_PATTERNS_TEXT, DTF_PATTERNS_ENDS, i))
}

/// Replaces each unquoted run of `z` with the letters for the requested zone name.
fn sub_zone(p: &str, letters: &str) -> String {
    let mut out = String::new();
    let mut quoted = false;
    let mut prev = '\0';
    for c in p.chars() {
        if c == '\'' {
            quoted = !quoted;
        }
        if !quoted && c == 'z' {
            if prev != 'z' {
                out.push_str(letters);
            }
        } else {
            out.push(c);
        }
        prev = if quoted { '\0' } else { c };
    }
    out
}

fn locale_pattern(l: &DateLocale, f: &[usize; 10], fsd: usize, cy: usize, styles: (Option<&str>, Option<&str>), tzn: Option<&str>) -> String {
    let (ds, ts) = styles;
    if ds.is_some() || ts.is_some() {
        let di = ds.map(|s| pos(&STYLES, Some(s)));
        let dp = di.and_then(|i| dtf_pattern(l.style_date[i]));
        let tp = ts.and_then(|s| dtf_pattern(l.style_time[cy][pos(&STYLES, Some(s))]));
        return match (dp, tp) {
            (Some(d), Some(t)) => join(l.style_glue[di.unwrap_or(3)], d, t),
            (Some(d), None) => d.to_string(),
            (None, Some(t)) => t.to_string(),
            (None, None) => String::new(),
        };
    }
    let dk = (((f[0] * 4 + f[1]) * 3 + f[2]) * 6 + f[3]) * 3 + f[4];
    let tk = ((((f[6] * 3 + f[7]) * 3 + f[8]) * 7 + cy) * 4 + f[5]) * 4 + fsd;
    let has_t = f[5] != 0 || f[6] != 0 || f[7] != 0 || f[8] != 0 || fsd != 0;
    let kind = tzn.map(|z| pos(&TZ_NAMES, Some(z)));
    let find = |t: &[(u16, u16)], k: usize| t.iter().find(|e| usize::from(e.0) == k).map(|e| dtf_str(DTF_PATTERNS_TEXT, DTF_PATTERNS_ENDS, e.1));
    let p: String = if !has_t {
        dtf_pattern(cell(if kind.is_some() { l.datez } else { l.date }, dk)).unwrap_or("").to_string()
    } else {
        let t = match kind {
            Some(k) => find(l.timez_ex[k], tk).or_else(|| dtf_pattern(cell(l.timez, tk))),
            None => dtf_pattern(cell(l.time, tk)),
        }
        .unwrap_or("");
        let weekday_only = f[0] != 0 && f[1] == 0 && f[2] == 0 && f[3] == 0 && f[4] == 0;
        let only_era = f[1] != 0 && f[0] == 0 && f[2] == 0 && f[3] == 0 && f[4] == 0;
        if dk == 0 {
            t.to_string()
        } else if let Some(w) = (kind.is_none() && weekday_only).then(|| find(l.weekday_ex, f[0] * 3024 + tk)).flatten() {
            w.to_string()
        } else {
            let d = if only_era { ["", "GGGG", "G", "GGGGG"][f[1]] } else { dtf_pattern(cell(l.date, dk)).unwrap_or("") };
            let glue = match f[3] {
                3 if f[0] != 0 => l.glue[0],
                3 => l.glue[1],
                4 => l.glue[2],
                _ => l.glue[3],
            };
            join(glue, d, t)
        }
    };
    match kind {
        Some(k) => sub_zone(&p, TZ_LETTERS[k]),
        None => p,
    }
}

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
    let dl = date_locale(locale);
    let cy = match (hour12, hc_opt) {
        (Some(true), _) => 1,
        (Some(false), _) => 2,
        (None, Some(c)) => 3 + pos(&CYCLES, Some(c)),
        (None, None) => 0,
    };
    let pattern = match (dl, ds, ts) {
        (Some(i), _, _) => locale_pattern(&DATE_LOCALES[i], &fields, fsd, cy, (ds, ts), tzn),
        (None, None, None) => match local.and_then(|l| local_pattern(l, &fields, fsd, hour12.is_some() || hc_opt.is_some() || tzn.is_some())) {
            Some(p) => p,
            None => compose(&fields, fsd, cycle, tzn),
        },
        (None, d, t) => {
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
    out.put("numberingSystem", numbering_system(locale));
    out.put("timeZone", &zone);
    let has = |c: char| field_width(&pattern, c) != 0;
    if has('j') {
        let hc = if has('h') { "h12" } else if has('K') { "h11" } else if has('k') { "h24" } else { "h23" };
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
    if let Some(i) = dl {
        out.put("@dl", &i.to_string());
    }
    out.put("@pattern", &pattern);
    Ok(out)
}

/// A locale's own pattern for numeric dates and times (the few locales with data), else None.
fn local_pattern(l: &LocaleDates, f: &[usize; 10], fsd: usize, custom: bool) -> Option<String> {
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
                || (letter == 'E' && c == 'c')
                || (letter == 'M' && c == 'L')
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
        if d.is_some()
            && f[6] == 0
            && let Some(i) = time.find("' \u{251c}AM/PM: '")
        {
            let end = time[i..].find('\u{2524}').map_or(time.len(), |j| i + j + '\u{2524}'.len_utf8());
            time.replace_range(i..end, "");
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

type ZoneEntry = (&'static str, &'static str, [u16; 6]);

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
fn zone_offset(zone: &str, t: i64) -> (i64, bool, Option<&'static ZoneEntry>) {
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
        (2, _) | (3, _) if minutes == 0 => if kind == 3 { "GMT+00:00" } else { "GMT+0" }.to_string(),
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

fn locale_offset(l: &DateLocale, minutes: i64, long: bool) -> String {
    let sign = if minutes < 0 { l.gmt[1] } else { "+" };
    let (h, m) = (minutes.abs() / 60, minutes.abs() % 60);
    if long {
        format!("{}{sign}{h:02}:{m:02}", l.gmt[0])
    } else if m == 0 {
        format!("{}{sign}{h}", l.gmt[0])
    } else {
        format!("{}{sign}{h}:{m:02}", l.gmt[0])
    }
}

/// Whether `t` falls in the repeated wall-clock hour before the zone's DST ends, where ICU
/// qualifies a generic name with the zone's location unless it is its metazone's golden zone.
fn in_overlap(zone: &str, t: i64) -> bool {
    let Some(z) = ZONES.iter().find(|z| z.0 == zone) else { return false };
    let r = rule(z.1);
    let Some((dst, _, b)) = r.dst else { return false };
    let (y, _, _) = civil((t + dst).div_euclid(86_400));
    let end = trans_local(b, y) - dst;
    t < end && end - t <= dst - r.std
}

fn locale_zone_name(l: &DateLocale, kind: usize, off: i64, dst: bool, zone: &str, overlap: bool) -> String {
    let minutes = off / 60;
    if kind == 2 || kind == 3 {
        return locale_offset(l, minutes, kind == 3);
    }
    let slot = match kind {
        0 => usize::from(dst),
        1 => 2 + usize::from(dst),
        4 if overlap => 6,
        4 => 4,
        _ if overlap => 7,
        _ => 5,
    };
    let at = |i: usize, slot: usize| l.zones.get(i * 8 + slot).copied().filter(|s| *s != u16::MAX);
    match ZONES.iter().position(|z| z.0 == zone).and_then(|i| at(i, slot).or_else(|| at(i, if slot >= 6 { slot - 2 } else { slot }))) {
        Some(s) => dtf_str(DTF_STRINGS_TEXT, DTF_STRINGS_ENDS, s).to_string(),
        None => locale_offset(l, minutes, kind == 1 || kind == 5),
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
    let dl = spec.get("@dl").and_then(|i| i.parse::<usize>().ok()).and_then(|i| DATE_LOCALES.get(i));
    let exact = (field_width(pattern, 'm') == 0 || mi == 0) && (field_width(pattern, 's') == 0 || s == 0);
    let width = |n: usize| match n {
        4 => 0,
        5 => 2,
        _ => 1,
    };
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
        if let Some(l) = dl {
            let standalone = usize::from(c == 'c' || c == 'L') * 3;
            let v = match c {
                'E' | 'c' => Some(("weekday", l.days[standalone + width(n)][wd].to_string())),
                'M' | 'L' if n >= 3 => Some(("month", l.months[standalone + width(n)][(mo - 1) as usize].to_string())),
                'G' => Some(("era", l.eras[width(n)][usize::from(y > 0)].to_string())),
                'a' => Some(("dayPeriod", l.ampm[usize::from(h >= 12)].to_string())),
                'B' => {
                    let k = match h {
                        0 if exact => 24,
                        12 if exact => 25,
                        _ => h as usize,
                    };
                    Some(("dayPeriod", l.periods[width(n)][k].to_string()))
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
                    let overlap = kind >= 4 && in_overlap(zone, secs);
                    Some(("timeZoneName", locale_zone_name(l, kind, off, dst, zone, overlap)))
                }
                _ => None,
            };
            if let Some((ty, v)) = v {
                out.push(ty, &v);
                continue;
            }
        }
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
                let alt = (kind >= 4 && in_overlap(zone, secs))
                    .then(|| ZONES.iter().position(|z| z.0 == zone))
                    .flatten()
                    .and_then(|i| EN_GENERIC_ALT.iter().find(|a| usize::from(a.0) == i));
                match alt {
                    Some(a) => out.push("timeZoneName", dtf_str(DTF_STRINGS_TEXT, DTF_STRINGS_ENDS, if kind == 4 { a.1 } else { a.2 })),
                    None => out.push("timeZoneName", &zone_name(kind, off, dst, entry)),
                }
            }
            _ => {}
        }
    }
    if spec.get("numberingSystem") == Some("arab") {
        for i in 1..out.items.len() {
            if out.items[i].0 == "fractionalSecond" && out.items[i - 1].0 == "literal" && out.items[i - 1].1.ends_with('.') {
                let lit = &mut out.items[i - 1].1;
                lit.pop();
                lit.push('\u{66b}');
            }
        }
        for (_, v) in out.items.iter_mut() {
            if v.bytes().any(|b| b.is_ascii_digit()) {
                *v = v.chars().map(|c| c.to_digit(10).and_then(|d| char::from_u32(0x660 + d)).unwrap_or(c)).collect();
            }
        }
    }
    Ok(out)
}
