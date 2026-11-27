use nanocode_sync::Atom;
use nanocode_sys::base64::STANDARD;

pub const ENV: &str = "HERDR_ENV";
const TERMINAL_VARIABLES: [&str; 2] = ["HC_TERMINAL_ID", "HC_TERMINAL_SOCKET"];
const MESSAGE_CHARS_MAX: usize = 200;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Permission,
    Question,
    Auth,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Idle,
    Working,
    Done,
    Blocked(Kind),
    Error,
}

impl State {
    const fn word(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Working => "working",
            Self::Done => "done",
            Self::Blocked(_) => "blocked",
            Self::Error => "error",
        }
    }

    const fn kind(self) -> Option<&'static str> {
        match self {
            Self::Blocked(Kind::Permission) => Some("permission"),
            Self::Blocked(Kind::Question) => Some("question"),
            Self::Blocked(Kind::Auth) => Some("auth"),
            _ => None,
        }
    }
}

pub fn enabled(lookup: &dyn Fn(&str) -> Option<String>) -> bool {
    let set = |name: &str| lookup(name).is_some_and(|value| !value.trim().is_empty());
    match lookup(ENV).map(|value| value.trim().to_ascii_lowercase()) {
        Some(value) if matches!(value.as_str(), "0" | "false" | "no") => false,
        Some(value) if matches!(value.as_str(), "1" | "true" | "yes") => true,
        _ => TERMINAL_VARIABLES.iter().any(|name| set(name)),
    }
}

#[must_use]
pub fn sequence(state: State, message: &str) -> String {
    let message = message
        .lines()
        .next()
        .unwrap_or_default()
        .chars()
        .take(MESSAGE_CHARS_MAX)
        .collect::<String>();
    let kind = state
        .kind()
        .map(|kind| format!(":kind={kind}"))
        .unwrap_or_default();
    format!(
        "\x1b]7501;state={}{kind}:msg={}\x1b\\",
        state.word(),
        STANDARD.encode(message)
    )
}

pub struct Reporter {
    enabled: bool,
    last: Atom<Option<(State, String)>>,
}

impl Reporter {
    #[must_use]
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            last: Atom::new(None),
        }
    }

    #[must_use]
    pub fn from_env() -> Self {
        Self::new(enabled(&|name| std::env::var(name).ok()))
    }

    #[must_use]
    pub const fn is_enabled(&self) -> bool {
        self.enabled
    }

    #[must_use]
    pub fn changed(&self, state: State, message: &str) -> Option<String> {
        if !self.enabled {
            return None;
        }
        let next = (state, message.to_owned());
        let changed = self.last.read(|last| last.as_ref() != Some(&next));
        changed.then(|| {
            self.last.set(Some(next));
            sequence(state, message)
        })
    }
}
