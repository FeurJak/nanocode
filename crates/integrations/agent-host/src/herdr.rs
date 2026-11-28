use nanocode_sync::Atom;
use nanocode_sys::base64::STANDARD;

pub const ENV: &str = "HERDR_ENV";
const TERMINAL_VARIABLES: [&str; 2] = ["HC_TERMINAL_ID", "HC_TERMINAL_SOCKET"];
const APP: &str = "nanocode";
pub const MESSAGE_BYTES_MAX: usize = 2048;
const CUT: char = '…';

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

pub mod sim {
    use std::sync::atomic::{AtomicU8, Ordering};

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    #[repr(u8)]
    pub enum Mutation {
        None,
        Uncapped,
        ControlsKept,
    }

    static MUTATION: AtomicU8 = AtomicU8::new(Mutation::None as u8);

    pub fn mutate(mutation: Mutation) {
        MUTATION.store(mutation as u8, Ordering::SeqCst);
    }

    pub fn mutated(mutation: Mutation) -> bool {
        MUTATION.load(Ordering::Relaxed) == mutation as u8
    }
}

fn line(text: &str) -> String {
    let controls = !sim::mutated(sim::Mutation::ControlsKept);
    text.lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or_default()
        .chars()
        .map(|character| match controls && character.is_control() {
            true => ' ',
            false => character,
        })
        .collect::<String>()
        .trim()
        .to_owned()
}

fn fitted(text: &str) -> String {
    let text = line(text);
    if text.len() <= MESSAGE_BYTES_MAX || sim::mutated(sim::Mutation::Uncapped) {
        return text;
    }
    let room = MESSAGE_BYTES_MAX - CUT.len_utf8();
    let end = (0..=room)
        .rev()
        .find(|index| text.is_char_boundary(*index))
        .unwrap_or_default();
    format!("{}{CUT}", &text[..end])
}

#[must_use]
pub fn sequence(state: State, message: &str) -> String {
    let kind = state
        .kind()
        .map(|kind| format!(":kind={kind}"))
        .unwrap_or_default();
    format!(
        "\x1b]7501;state={}{kind}:app={APP}:msg={}\x1b\\",
        state.word(),
        STANDARD.encode(fitted(message))
    )
}

#[must_use]
pub fn title(text: &str) -> String {
    format!("\x1b]2;{}\x1b\\", line(text))
}

pub struct Reporter {
    enabled: bool,
    last: Atom<Option<(State, String)>>,
    titled: Atom<Option<String>>,
}

impl Reporter {
    #[must_use]
    pub fn new(enabled: bool) -> Self {
        Self {
            enabled,
            last: Atom::new(None),
            titled: Atom::new(None),
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

    #[must_use]
    pub fn retitled(&self, text: &str) -> Option<String> {
        if !self.enabled {
            return None;
        }
        let next = line(text);
        let changed = self
            .titled
            .read(|titled| titled.as_deref() != Some(next.as_str()));
        changed.then(|| {
            let sequence = title(&next);
            self.titled.set(Some(next));
            sequence
        })
    }
}
