use nanocode_backend_claude_client::ToolResultContent;
use nanocode_tool_host::code_mode_spec::EXEC_PRAGMA_PREFIX;
pub use nanocode_tool_host::deadline::Deadline;
pub(crate) use nanocode_tool_host::deadline::{Cap, Reminders};
use serde_json::Value;
use std::borrow::Cow;
use std::time::Duration;

pub(crate) fn cap(deadline: Deadline, name: &str, input: &Value) -> Cap<Value> {
    match name {
        "Bash" => deadline.cap("timeout", input),
        "wait" => deadline.cap("yield_time_ms", input),
        "exec" if input.get("yield_time_ms").and_then(Value::as_f64).is_some() => {
            deadline.cap("yield_time_ms", input)
        }
        "exec" => {
            let (Some(object), Some(code)) =
                (input.as_object(), input.get("code").and_then(Value::as_str))
            else {
                return Cap::Unchanged;
            };
            match deadline.cap_source(code) {
                Cap::Capped {
                    input: code,
                    notice,
                } => {
                    let mut object = object.clone();
                    object.insert("code".into(), code.into());
                    Cap::Capped {
                        input: Value::Object(object),
                        notice,
                    }
                }
                Cap::Expired(text) => Cap::Expired(text),
                Cap::Unchanged => Cap::Unchanged,
            }
        }
        _ => Cap::Unchanged,
    }
}

pub(crate) fn share(deadline: Deadline, percent: u8, name: &str, input: &Value) -> Cap<Value> {
    match name {
        "wait" => deadline.cap_share(percent, "yield_time_ms", input),
        "exec" if input.get("yield_time_ms").and_then(Value::as_f64).is_some() => {
            deadline.cap_share(percent, "yield_time_ms", input)
        }
        "exec" => {
            let (Some(object), Some(code)) =
                (input.as_object(), input.get("code").and_then(Value::as_str))
            else {
                return Cap::Unchanged;
            };
            match deadline.cap_source_share(percent, code) {
                Cap::Capped {
                    input: code,
                    notice,
                } => {
                    let mut object = object.clone();
                    object.insert("code".into(), code.into());
                    Cap::Capped {
                        input: Value::Object(object),
                        notice,
                    }
                }
                Cap::Expired(text) => Cap::Expired(text),
                Cap::Unchanged => Cap::Unchanged,
            }
        }
        _ => Cap::Unchanged,
    }
}

pub(crate) fn append(content: &mut ToolResultContent, notice: &str) {
    match content {
        ToolResultContent::Text(text) => {
            text.push_str("\n\n");
            text.push_str(notice);
        }
        ToolResultContent::Blocks(blocks) => {
            blocks.push(serde_json::json!({"type": "text", "text": notice}));
        }
    }
}

pub(crate) fn floor(floor: Duration, name: &str, input: &Value) -> Option<Value> {
    let ms = u64::try_from(floor.as_millis()).unwrap_or(u64::MAX);
    let below = |value: Option<&Value>| {
        value
            .and_then(Value::as_f64)
            .is_none_or(|given| given < ms as f64)
    };
    let mut object = input.as_object()?.clone();
    match name {
        "wait" => {
            if object.get("terminate").and_then(Value::as_bool) == Some(true)
                || !below(object.get("yield_time_ms"))
            {
                return None;
            }
            object.insert("yield_time_ms".into(), ms.into());
        }
        "exec"
            if object
                .get("yield_time_ms")
                .is_some_and(|value| !value.is_null()) =>
        {
            if !below(object.get("yield_time_ms")) {
                return None;
            }
            object.insert("yield_time_ms".into(), ms.into());
        }
        "exec" => {
            let code = object.get("code").and_then(Value::as_str)?;
            let code = floor_source(code, floor, ms)?;
            object.insert("code".into(), code.into());
        }
        _ => return None,
    }
    Some(Value::Object(object))
}

fn floor_source(code: &str, floor: Duration, ms: u64) -> Option<String> {
    let (first, rest) = code.split_once('\n').unwrap_or((code, ""));
    if let Some(pragma) = first.trim_start().strip_prefix(EXEC_PRAGMA_PREFIX)
        && let Ok(Value::Object(mut pragma)) = serde_json::from_str::<Value>(pragma)
        && pragma
            .get("yield_time_ms")
            .and_then(Value::as_f64)
            .is_some_and(|given| given < ms as f64)
    {
        pragma.insert("yield_time_ms".into(), ms.into());
        return Some(format!(
            "{EXEC_PRAGMA_PREFIX} {}\n{rest}",
            Value::Object(pragma)
        ));
    }
    match nanocode_tool_host::code_mode::source_yielding(code, floor) {
        Cow::Owned(code) => Some(code),
        Cow::Borrowed(_) => None,
    }
}
