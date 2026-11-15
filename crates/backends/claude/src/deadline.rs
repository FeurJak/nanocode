use nanocode_backend_claude_client::ToolResultContent;
use nanocode_code_mode::code_mode_spec::EXEC_PRAGMA_PREFIX;
use nanocode_sync::Atom;
use serde_json::{Map, Value};
use std::time::Duration;

const REMINDER_PERCENTS: [u128; 3] = [50, 75, 90];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Deadline {
    at_unix_ms: u64,
    limit: Duration,
}

pub(crate) enum Cap {
    Capped { input: Value, notice: String },
    Expired(String),
    Unchanged,
}

pub(crate) struct Reminders {
    announced: Atom<usize>,
    deadline: Deadline,
}

impl Deadline {
    #[must_use]
    pub const fn new(at_unix_ms: u64, limit: Duration) -> Self {
        Self { at_unix_ms, limit }
    }

    fn remaining(self) -> Duration {
        Duration::from_millis(
            self.at_unix_ms
                .saturating_sub(nanocode_platform::unix_millis()),
        )
    }

    fn passed(self, remaining: Duration) -> usize {
        let elapsed = self.limit.saturating_sub(remaining).as_millis();
        let limit = self.limit.as_millis();
        REMINDER_PERCENTS
            .iter()
            .filter(|percent| elapsed * 100 >= limit * **percent)
            .count()
    }

    pub(crate) fn cap(self, name: &str, input: &Value) -> Cap {
        let field = match name {
            "Bash" => "timeout",
            "exec" | "wait" => "yield_time_ms",
            _ => return Cap::Unchanged,
        };
        let remaining = self.remaining();
        let allowed = u64::try_from(remaining.as_millis()).unwrap_or(u64::MAX);
        let Some(mut object) = input.as_object().cloned() else {
            return Cap::Unchanged;
        };
        let requested = match object.get(field).and_then(Value::as_f64) {
            Some(requested) => requested,
            None if name == "exec" => {
                return pragma_cap(&object, allowed).map_or(Cap::Unchanged, |(code, requested)| {
                    object.insert("code".into(), code.into());
                    self.capped(Value::Object(object), (field, requested, allowed))
                });
            }
            None => return Cap::Unchanged,
        };
        if requested <= allowed as f64 {
            return Cap::Unchanged;
        }
        object.insert(field.into(), allowed.into());
        self.capped(Value::Object(object), (field, requested, allowed))
    }

    fn capped(self, input: Value, request: (&str, f64, u64)) -> Cap {
        let (field, requested, allowed) = request;
        if allowed == 0 {
            return Cap::Expired(format!(
                "The {} time limit for this task is reached, so this call did not run.",
                spoken(self.limit)
            ));
        }
        Cap::Capped {
            input,
            notice: format!(
                "{field} {requested} ms is longer than the {} left before this task's {} time limit, so this call ran with {field} {allowed} ms.",
                spoken(Duration::from_millis(allowed)),
                spoken(self.limit)
            ),
        }
    }
}

fn pragma_cap(object: &Map<String, Value>, allowed: u64) -> Option<(String, f64)> {
    let code = object.get("code")?.as_str()?;
    let (first, rest) = code.split_once('\n').unwrap_or((code, ""));
    let pragma = first.trim_start().strip_prefix(EXEC_PRAGMA_PREFIX)?;
    let Value::Object(mut pragma) = serde_json::from_str::<Value>(pragma).ok()? else {
        return None;
    };
    let requested = pragma.get("yield_time_ms")?.as_f64()?;
    if requested <= allowed as f64 {
        return None;
    }
    pragma.insert("yield_time_ms".into(), allowed.into());
    Some((
        format!("{EXEC_PRAGMA_PREFIX} {}\n{rest}", Value::Object(pragma)),
        requested,
    ))
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

impl Reminders {
    pub(crate) fn new(deadline: Deadline) -> Self {
        Self {
            announced: Atom::new(0),
            deadline,
        }
    }

    pub(crate) fn notice(&self) -> Option<String> {
        let remaining = self.deadline.remaining();
        let passed = self.deadline.passed(remaining);
        let announced = self.announced.update(|announced| {
            let was = *announced;
            *announced = was.max(passed);
            was
        });
        (passed > announced).then(|| {
            format!(
                "Time limit: {} of this task's {} remain. When the time is up the task stops, and only what is on disk then counts. Make sure every output the task asks for exists in its final location now, before you spend time improving it. Do not start a command or a wait that cannot finish in the time that remains.",
                spoken(remaining),
                spoken(self.deadline.limit)
            )
        })
    }
}

fn spoken(duration: Duration) -> String {
    let seconds = duration.as_secs();
    match (seconds / 60, seconds % 60) {
        (0, seconds) => format!("{seconds} s"),
        (minutes, 0) => format!("{minutes} min"),
        (minutes, seconds) => format!("{minutes} min {seconds} s"),
    }
}
