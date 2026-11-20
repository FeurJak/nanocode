use super::data::{LISTS, RELATIVE, RELATIVE_AUTO};
use super::number::{self, Num};
use super::{Parts, Spec, plural_categories, range, type_err};
use crate::interp::conv::Fault;
use alloc::format;

const UNITS: [&str; 8] = ["second", "minute", "hour", "day", "week", "month", "quarter", "year"];

pub(super) fn resolve(kind: i32, locale: &str, raw: &Spec) -> Result<Spec, Fault> {
    let mut out = Spec::default();
    out.put("locale", locale);
    let lang = locale.split('-').next().unwrap_or("en");
    match kind {
        3 => {
            let ty = raw.choose("type", &["cardinal", "ordinal"], "cardinal", "Intl.PluralRules")?;
            out.put("type", ty);
            number::digit_options(raw, &mut out, (0, 3), false)?;
            out.flush_later();
            out.put("pluralCategories", &format!("@{}", plural_categories(lang, ty == "ordinal")));
        }
        4 => {
            let n = "Intl.ListFormat";
            let ty = raw.choose("type", &["conjunction", "disjunction", "unit"], "conjunction", n)?;
            let st = raw.choose("style", &["long", "short", "narrow"], "long", n)?;
            out.put("type", ty);
            out.put("style", st);
        }
        5 => {
            let n = "Intl.RelativeTimeFormat";
            let st = raw.choose("style", &["long", "short", "narrow"], "long", n)?;
            let nu = raw.choose("numeric", &["always", "auto"], "always", n)?;
            out.put("style", st);
            out.put("numeric", nu);
            out.put("numberingSystem", "latn");
        }
        _ => {
            let g = raw.choose("granularity", &["grapheme", "word", "sentence"], "grapheme", "Intl.Segmenter")?;
            out.put("granularity", g);
        }
    }
    Ok(out)
}

fn index(values: &[&str], v: Option<&str>) -> u8 {
    values.iter().position(|x| Some(*x) == v).unwrap_or(0) as u8
}

fn unit_index(u: &str) -> Option<usize> {
    let u = u.strip_suffix('s').unwrap_or(u);
    UNITS.iter().position(|x| *x == u)
}

pub(super) fn format(kind: i32, spec: &Spec, n: Option<f64>, s: Option<&str>, y: Option<&str>) -> Result<Parts, Fault> {
    let locale = spec.get("locale").unwrap_or("en-US");
    let lang = locale.split('-').next().unwrap_or("en");
    let mut out = Parts::default();
    match kind {
        3 => {
            let x = match n {
                Some(v) => number::from_f64(v),
                None => return Ok(out),
            };
            let cat = match x {
                Num::Dec(d) => {
                    let g = number::Digits {
                        min_int: 1,
                        min_fd: spec.int("minimumFractionDigits", 0),
                        max_fd: spec.int("maximumFractionDigits", 3),
                        min_sd: spec.int("minimumSignificantDigits", 1),
                        max_sd: spec.int("maximumSignificantDigits", 21),
                        kind: if spec.get("minimumSignificantDigits").is_some() { "significantDigits" } else { "fractionDigits" },
                        priority: "auto",
                    };
                    let r = number::round(&d, &g, "halfExpand", false);
                    number::plural_of(lang, &r, spec.get("type") == Some("ordinal"))
                }
                _ => "other",
            };
            out.push("literal", cat);
        }
        4 => {
            let items = s.unwrap_or("");
            let list: alloc::vec::Vec<&str> = if items.is_empty() { alloc::vec::Vec::new() } else { items.split('\u{1}').collect() };
            let ty = index(&["conjunction", "disjunction", "unit"], spec.get("type"));
            let st = index(&["long", "short", "narrow"], spec.get("style"));
            let row = LISTS
                .iter()
                .find(|r| r.0 == lang && r.1 == ty && r.2 == st)
                .or_else(|| LISTS.iter().find(|r| r.0 == "en" && r.1 == ty && r.2 == st))
                .map_or([", "; 4], |r| r.3);
            let mut sep = row[3];
            if lang == "es" && ty == 0 && st < 2 && list.last().is_some_and(|w| w.starts_with('i') || w.starts_with("hi") && !w.starts_with("hie")) {
                sep = " e ";
            }
            for (i, item) in list.iter().enumerate() {
                if i > 0 {
                    let lit = if list.len() == 2 { row[0] } else if i == list.len() - 1 { sep } else if i == 1 { row[1] } else { row[2] };
                    out.push("literal", lit);
                }
                out.push("element", item);
            }
        }
        5 => {
            let Some(v) = n else { return range("Invalid number value") };
            if !v.is_finite() {
                return range("Invalid time value");
            }
            let unit = y.unwrap_or("");
            let Some(ui) = unit_index(unit) else {
                return range(&format!("Invalid unit argument for format() '{unit}'"));
            };
            let st = index(&["long", "short", "narrow"], spec.get("style"));
            let st_l = if lang == "en" { st } else { 0 };
            let neg = v < 0.0 || (v == 0.0 && v.is_sign_negative());
            if spec.get("numeric") == Some("auto")
                && v == (v as i64) as f64
                && let Some(r) = RELATIVE_AUTO.iter().find(|r| r.0 == lang && usize::from(r.1) == ui && r.2 == st_l && f64::from(r.3) == v)
            {
                out.push("literal", r.4);
                return Ok(out);
            }
            let mut nspec = Spec::default();
            nspec.put("locale", locale);
            let f = number::format(&nspec, number::from_f64(v.abs()));
            let plural = {
                let d = number::from_f64(v.abs());
                match d {
                    Num::Dec(d) => {
                        let g = number::Digits { min_int: 1, min_fd: 0, max_fd: 3, min_sd: 1, max_sd: 21, kind: "fractionDigits", priority: "auto" };
                        number::plural_of(lang, &number::round(&d, &g, "halfExpand", false), false)
                    }
                    _ => "other",
                }
            };
            let dir = u8::from(neg);
            let pick = |l: &str| {
                RELATIVE
                    .iter()
                    .find(|r| r.0 == l && usize::from(r.1) == ui && r.2 == st_l && r.3 == dir && r.4 == plural)
                    .or_else(|| RELATIVE.iter().find(|r| r.0 == l && usize::from(r.1) == ui && r.2 == st_l && r.3 == dir && r.4 == "other"))
            };
            let pat = pick(lang).or_else(|| pick("en")).map_or("{0}", |r| r.5);
            let (pre, post) = pat.split_once("{0}").unwrap_or((pat, ""));
            out.push("literal", pre);
            out.extend(&f.parts);
            out.push("literal", post);
        }
        _ => return type_err("unsupported Intl kind"),
    }
    Ok(out)
}
