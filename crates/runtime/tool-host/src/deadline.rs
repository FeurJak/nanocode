use nanocode_code_mode::code_mode_spec::EXEC_PRAGMA_PREFIX;
use nanocode_sync::Atom;
use serde_json::Value;
use std::time::Duration;

const EARLY_REMINDER: &str = "Keep working until then. First put every output the task asks for in its final location, then use the time that remains to improve and test it. Test it on an input you made that differs from the given example, because the task's checks use inputs you have not seen.";
const LAST_REMINDER: &str = "Finish now. Make sure every output the task asks for is in its final location. Delete only the scratch files and test output you made where the task's program or its checks write, read or wait for files, such as a file in /tmp. Keep what the task's program built or set up: caches and compiled artifacts (such as a compiled model or a build directory), installed packages, and the state the task asked for (repositories and their branches, served files, databases, services). Never reset or empty that state to make it clean. Stop background processes you started, unless the task asked to keep them running. Do not start a command or a wait that cannot finish in the time that remains.";
const FINISH_OPEN: &str =
    "<system-reminder>\nThis session is non-interactive and nobody will reply.";
const FINISH_CHECK: &str = " Before you finish, check the work:\n";
const FINISH_CHECKLIST: &str = "- Every output the task names exists at that exact path, with a relative path resolved against the working directory.\n- The task's own check, or a direct test of the result, passes when you run it again now.\n- The task's checks use inputs you have not seen (other files, data, sizes or seeds) and run the program as a script does: stdout piped, not a terminal, and signals such as SIGINT. The result holds on at least one input you made that differs from the given example, including the case the task names, and with stdout piped when output matters.\n- The scratch files and test output you made where the task's program or its checks write, read or wait for files, such as a file in /tmp, are deleted, and nothing else is.\n- What the task's program built or set up is kept: caches and compiled artifacts (such as a compiled model or a build directory), installed packages, and the state the task asked for (repositories and their branches, served files, databases, services). None of that state is reset or emptied to make it clean.\n- Background processes you started are stopped, unless the task asked to keep them running.\n- Any choice left open is made, and its result is written.\n";
const TIME_LIMIT: &str = "Time limit: ";
const TIME_UP: &str = " When the time is up the task stops, and only what is on disk then counts.";
const REMINDER_PERCENTS: [u128; 3] = [50, 75, 90];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Deadline {
    at_unix_ms: u64,
    limit: Duration,
}

pub enum Cap<T> {
    Capped { input: T, notice: String },
    Expired(String),
    Unchanged,
}

pub struct Reminders {
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

    fn allowed(self) -> u64 {
        u64::try_from(self.remaining().as_millis()).unwrap_or(u64::MAX)
    }

    fn passed(self, remaining: Duration) -> usize {
        let elapsed = self.limit.saturating_sub(remaining).as_millis();
        let limit = self.limit.as_millis();
        REMINDER_PERCENTS
            .iter()
            .filter(|percent| elapsed * 100 >= limit * **percent)
            .count()
    }

    fn remain(self, remaining: Duration) -> String {
        format!(
            "{} of this task's {} remain.",
            spoken(remaining),
            spoken(self.limit)
        )
    }

    #[must_use]
    pub fn cap(self, field: &str, input: &Value) -> Cap<Value> {
        let Some(mut object) = input.as_object().cloned() else {
            return Cap::Unchanged;
        };
        let Some(requested) = object.get(field).and_then(Value::as_f64) else {
            return Cap::Unchanged;
        };
        let allowed = self.allowed();
        if requested <= allowed as f64 {
            return Cap::Unchanged;
        }
        object.insert(field.into(), allowed.into());
        self.capped(Value::Object(object), (field, requested, allowed))
    }

    #[must_use]
    pub fn cap_source(self, code: &str) -> Cap<String> {
        let allowed = self.allowed();
        pragma_cap(code, allowed).map_or(Cap::Unchanged, |(code, requested)| {
            self.capped(code, ("yield_time_ms", requested, allowed))
        })
    }

    fn capped<T>(self, input: T, request: (&str, f64, u64)) -> Cap<T> {
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

fn pragma_cap(code: &str, allowed: u64) -> Option<(String, f64)> {
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

impl Reminders {
    #[must_use]
    pub fn new(deadline: Deadline) -> Self {
        Self {
            announced: Atom::new(0),
            deadline,
        }
    }

    #[must_use]
    pub const fn deadline(&self) -> Deadline {
        self.deadline
    }

    #[must_use]
    pub fn first_passed(&self) -> bool {
        self.deadline.passed(self.deadline.remaining()) > 0
    }

    #[must_use]
    pub fn last_passed(&self) -> bool {
        self.deadline.passed(self.deadline.remaining()) == REMINDER_PERCENTS.len()
    }

    #[must_use]
    pub fn notice(&self) -> Option<String> {
        let remaining = self.deadline.remaining();
        let passed = self.deadline.passed(remaining);
        let announced = self.announced.update(|announced| {
            let was = *announced;
            *announced = was.max(passed);
            was
        });
        if passed == 0 {
            return None;
        }
        let remain = self.deadline.remain(remaining);
        if passed <= announced {
            return Some(format!("{TIME_LIMIT}{remain}"));
        }
        let steps = if passed == REMINDER_PERCENTS.len() {
            LAST_REMINDER
        } else {
            EARLY_REMINDER
        };
        Some(format!("{TIME_LIMIT}{remain}{TIME_UP} {steps}"))
    }

    #[must_use]
    pub fn remain(&self) -> String {
        self.deadline.remain(self.deadline.remaining())
    }

    #[must_use]
    pub fn left(&self) -> Duration {
        self.deadline.remaining()
    }
}

#[must_use]
pub fn finish_notice(deadline: Option<&Reminders>, brief: bool) -> String {
    let time = deadline.map_or_else(String::new, |reminders| format!(" {}", reminders.remain()));
    let reply = if brief {
        "reply with one line that says the task is done"
    } else {
        "reply with your final message"
    };
    format!(
        "{FINISH_OPEN}{time}{FINISH_CHECK}{FINISH_CHECKLIST}Fix what falls short. When everything holds, {reply}.\n</system-reminder>"
    )
}

#[must_use]
pub fn folded_finish_notice() -> String {
    format!(
        "{FINISH_OPEN} Before your final message, check the work:\n{FINISH_CHECKLIST}Fix what falls short before you reply with your final message.\n</system-reminder>"
    )
}

#[must_use]
pub fn is_finish_notice(text: &str) -> bool {
    text.starts_with(FINISH_OPEN) && text.contains(FINISH_CHECK)
}

#[must_use]
pub fn is_time_left(text: &str) -> bool {
    text.starts_with(TIME_LIMIT) && !text.contains(TIME_UP)
}

fn spoken(duration: Duration) -> String {
    let seconds = duration.as_secs();
    match (seconds / 60, seconds % 60) {
        (0, seconds) => format!("{seconds} s"),
        (minutes, 0) => format!("{minutes} min"),
        (minutes, seconds) => format!("{minutes} min {seconds} s"),
    }
}
