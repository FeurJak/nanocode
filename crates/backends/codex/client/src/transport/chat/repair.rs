use serde_json::{Map, Value};

pub(super) fn arguments(text: &str) -> Option<Value> {
    let trimmed = unfence(text.trim());
    if trimmed.is_empty() {
        return Some(Value::Object(Map::new()));
    }
    first_value(trimmed).or_else(|| first_value(&relax(trimmed)))
}

pub(super) fn decoded(value: Value) -> Value {
    match &value {
        Value::String(inner) => match serde_json::from_str::<Value>(inner.trim()) {
            Ok(object @ Value::Object(_)) => object,
            _ => value,
        },
        _ => value,
    }
}

pub(super) fn conform(arguments: &mut Map<String, Value>, parameters: &Value) -> bool {
    let Some(properties) = parameters.get("properties").and_then(Value::as_object) else {
        return false;
    };
    let mut repaired = false;
    for (key, schema) in properties {
        match arguments.get_mut(key) {
            None => {
                if let Some(default) = schema.get("default") {
                    arguments.insert(key.clone(), default.clone());
                    repaired = true;
                }
            }
            Some(value) => {
                if let Some(coerced) = coerce(value, schema) {
                    *value = coerced;
                    repaired = true;
                }
            }
        }
    }
    repaired
}

fn coerce(value: &Value, schema: &Value) -> Option<Value> {
    let Value::String(text) = value else {
        return None;
    };
    let types: Vec<&str> = match schema.get("type")? {
        Value::String(kind) => vec![kind.as_str()],
        Value::Array(kinds) => kinds.iter().filter_map(Value::as_str).collect(),
        _ => return None,
    };
    if types.contains(&"string") {
        return None;
    }
    let parsed: Value = serde_json::from_str(text.trim()).ok()?;
    let fits = |kind: &&str| match *kind {
        "integer" => parsed.is_i64() || parsed.is_u64(),
        "number" => parsed.is_number(),
        "boolean" => parsed.is_boolean(),
        "array" => parsed.is_array(),
        "object" => parsed.is_object(),
        _ => false,
    };
    types.iter().any(fits).then_some(parsed)
}

pub(super) fn name(returned: &str) -> &str {
    let trimmed = returned.trim();
    let trimmed = trimmed.strip_prefix("functions.").unwrap_or(trimmed);
    match trimmed.rsplit_once(':') {
        Some((name, index))
            if !name.is_empty()
                && !index.is_empty()
                && index.bytes().all(|byte| byte.is_ascii_digit()) =>
        {
            name
        }
        _ => trimmed,
    }
}

fn unfence(text: &str) -> &str {
    let Some(body) = text
        .strip_prefix("```")
        .and_then(|rest| rest.strip_suffix("```"))
    else {
        return text;
    };
    match body.split_once('\n') {
        Some((tag, inner)) if !tag.trim_start().starts_with(['{', '[']) => inner.trim(),
        _ => body.trim(),
    }
}

fn first_value(text: &str) -> Option<Value> {
    let mut values = serde_json::Deserializer::from_str(text).into_iter::<Value>();
    let value = values.next()?.ok()?;
    text[values.byte_offset()..]
        .chars()
        .all(|c| c.is_whitespace() || matches!(c, '}' | ']'))
        .then_some(value)
}

fn relax(text: &str) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::with_capacity(text.len());
    let mut index = 0;
    while index < chars.len() {
        let c = chars[index];
        if c == '"' || c == '\'' {
            index = string(&chars, index, &mut out);
        } else if c == ',' {
            let next = chars[index + 1..].iter().find(|c| !c.is_whitespace());
            if !matches!(next, None | Some('}' | ']')) {
                out.push(',');
            }
            index += 1;
        } else if c.is_ascii_alphabetic() || c == '_' {
            let start = index;
            while index < chars.len()
                && (chars[index].is_ascii_alphanumeric() || chars[index] == '_')
            {
                index += 1;
            }
            let word: String = chars[start..index].iter().collect();
            if chars[index..].iter().find(|c| !c.is_whitespace()) == Some(&':') {
                out.push('"');
                out.push_str(&word);
                out.push('"');
            } else {
                out.push_str(match word.as_str() {
                    "True" => "true",
                    "False" => "false",
                    "None" => "null",
                    other => other,
                });
            }
        } else {
            out.push(c);
            index += 1;
        }
    }
    out
}

fn string(chars: &[char], start: usize, out: &mut String) -> usize {
    let quote = chars[start];
    out.push('"');
    let mut index = start + 1;
    while index < chars.len() {
        let c = chars[index];
        if c == quote {
            out.push('"');
            return index + 1;
        }
        match c {
            '\\' => {
                match chars.get(index + 1) {
                    Some(&next @ ('"' | '\\' | '/' | 'b' | 'f' | 'n' | 'r' | 't' | 'u')) => {
                        out.push('\\');
                        out.push(next);
                    }
                    Some('\'') => out.push('\''),
                    Some(&next) => {
                        out.push_str("\\\\");
                        out.push(next);
                    }
                    None => out.push_str("\\\\"),
                }
                index += 2;
                continue;
            }
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if u32::from(c) < 0x20 => out.push_str(&format!("\\u{:04x}", u32::from(c))),
            c => out.push(c),
        }
        index += 1;
    }
    index
}
