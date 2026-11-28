use std::{ops::Range, process::ExitCode};

use nanocode::herdr::{
    Kind, MESSAGE_BYTES_MAX, Reporter, State, sequence,
    sim::{self, Mutation as HerdrRule},
    title,
};
use nanocode_sys::base64::STANDARD;

use crate::rng::{FNV_OFFSET, Rng, fnv};

const STEPS_MIN: u64 = 4;
const STEPS_MAX: u64 = 24;
const LONG_PPM: u64 = 200_000;
const PIECES: [&str; 12] = [
    "a", "Z", " ", "\t", "\n", "\r\n", "é", "中", "🦀", "\u{1b}", "\u{7}", "\u{0}",
];
const STATES: [State; 7] = [
    State::Idle,
    State::Working,
    State::Done,
    State::Error,
    State::Blocked(Kind::Permission),
    State::Blocked(Kind::Question),
    State::Blocked(Kind::Auth),
];
const PREFIX: &str = "\u{1b}]7501;";
const SUFFIX: &str = "\u{1b}\\";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HerdrMutation {
    None,
    Uncapped,
    ControlsKept,
}

impl HerdrMutation {
    const ALL: [Self; 3] = [Self::None, Self::Uncapped, Self::ControlsKept];

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|mutation| mutation.name() == name)
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Uncapped => "uncapped",
            Self::ControlsKept => "controls_kept",
        }
    }

    pub fn names() -> String {
        Self::ALL
            .iter()
            .skip(1)
            .map(|mutation| mutation.name())
            .collect::<Vec<_>>()
            .join("|")
    }

    fn install(self) {
        sim::mutate(match self {
            Self::None => HerdrRule::None,
            Self::Uncapped => HerdrRule::Uncapped,
            Self::ControlsKept => HerdrRule::ControlsKept,
        });
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct HerdrStats {
    pub seeds: u64,
    pub reports: u64,
    pub repeats: u64,
    pub cut: u64,
    pub controls: u64,
    pub titles: u64,
}

impl HerdrStats {
    pub const fn add(&mut self, other: &Self) {
        self.seeds += other.seeds;
        self.reports += other.reports;
        self.repeats += other.repeats;
        self.cut += other.cut;
        self.controls += other.controls;
        self.titles += other.titles;
    }
}

fn text(rng: &mut Rng) -> String {
    let (pieces, breaks) = match rng.chance(LONG_PPM) {
        true => (
            rng.range(MESSAGE_BYTES_MAX as u64 / 2, MESSAGE_BYTES_MAX as u64 * 3),
            false,
        ),
        false => (rng.range(0, 40), true),
    };
    (0..pieces)
        .map(|_| PIECES[rng.pick(PIECES.len())])
        .filter(|piece| breaks || !piece.contains('\n'))
        .collect()
}

fn expected_line(text: &str) -> String {
    text.lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or_default()
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>()
        .trim()
        .to_owned()
}

fn word(state: State) -> (&'static str, Option<&'static str>) {
    match state {
        State::Idle => ("idle", None),
        State::Working => ("working", None),
        State::Done => ("done", None),
        State::Error => ("error", None),
        State::Blocked(Kind::Permission) => ("blocked", Some("permission")),
        State::Blocked(Kind::Question) => ("blocked", Some("question")),
        State::Blocked(Kind::Auth) => ("blocked", Some("auth")),
    }
}

fn check(state: State, message: &str, report: &str, stats: &mut HerdrStats) -> Result<(), String> {
    let body = report
        .strip_prefix(PREFIX)
        .and_then(|rest| rest.strip_suffix(SUFFIX))
        .ok_or_else(|| format!("report {report:?} is not one OSC 7501 sequence ended by ST"))?;
    if !body.bytes().all(|byte| (0x20..0x7f).contains(&byte)) {
        return Err(format!(
            "report body {body:?} is not printable ASCII, so not escape-safe"
        ));
    }
    let (state_word, kind) = word(state);
    let mut fields = vec![format!("state={state_word}")];
    fields.extend(kind.map(|kind| format!("kind={kind}")));
    fields.push("app=nanocode".to_owned());
    let head = fields.join(":") + ":msg=";
    let encoded = body
        .strip_prefix(&head)
        .ok_or_else(|| format!("report body {body:?} does not start with {head:?}"))?;
    let decoded = STANDARD
        .decode(encoded)
        .map_err(|error| format!("msg {encoded:?} is not base64: {error}"))?;
    let decoded = String::from_utf8(decoded)
        .map_err(|error| format!("msg of {message:?} is not UTF-8: {error}"))?;
    if decoded.len() > MESSAGE_BYTES_MAX {
        return Err(format!(
            "msg is {} bytes, over the {MESSAGE_BYTES_MAX} bytes Herdr accepts, so Herdr drops the report",
            decoded.len()
        ));
    }
    if decoded.chars().any(char::is_control) {
        stats.controls += 1;
        return Err(format!("msg {decoded:?} carries control characters"));
    }
    let expected = expected_line(message);
    if expected.len() <= MESSAGE_BYTES_MAX {
        if decoded != expected {
            return Err(format!(
                "msg {decoded:?} is not the first line {expected:?}"
            ));
        }
        return Ok(());
    }
    stats.cut += 1;
    let kept = decoded
        .strip_suffix('…')
        .ok_or_else(|| "a cut msg does not end with …".to_owned())?;
    if !expected.starts_with(kept) || kept.len() + 4 + '…'.len_utf8() <= MESSAGE_BYTES_MAX {
        return Err(format!(
            "a cut msg keeps {} bytes of the line, not its longest prefix that fits",
            kept.len()
        ));
    }
    Ok(())
}

fn run(seed: u64, stats: &mut HerdrStats) -> Result<u64, String> {
    let mut rng = Rng::derive(seed, "herdr");
    let reporter = Reporter::new(true);
    let mut last: Option<(State, String)> = None;
    let mut digest = FNV_OFFSET;
    for _ in 0..rng.range(STEPS_MIN, STEPS_MAX) {
        let state = STATES[rng.pick(STATES.len())];
        let message = match (&last, rng.chance(300_000)) {
            (Some((_, previous)), true) => previous.clone(),
            _ => text(&mut rng),
        };
        let next = (state, message.clone());
        let report = reporter.changed(state, &message);
        match (last.as_ref() == Some(&next), report) {
            (true, Some(report)) => {
                return Err(format!(
                    "an unchanged status was reported again: {report:?}"
                ));
            }
            (true, None) => stats.repeats += 1,
            (false, None) => return Err(format!("a changed status {next:?} was not reported")),
            (false, Some(report)) => {
                check(state, &message, &report, stats)?;
                check(state, &message, &sequence(state, &message), stats)?;
                stats.reports += 1;
                digest = fnv(digest, report.as_bytes());
            }
        }
        last = Some(next);
        let heading = text(&mut rng);
        let titled = title(&heading);
        let inner = titled
            .strip_prefix("\u{1b}]2;")
            .and_then(|rest| rest.strip_suffix(SUFFIX))
            .ok_or_else(|| format!("title {titled:?} is not one OSC 2 sequence"))?;
        if inner.chars().any(char::is_control) || inner != expected_line(&heading) {
            return Err(format!(
                "title {inner:?} is not the first line of {heading:?} without control characters"
            ));
        }
        stats.titles += 1;
    }
    Ok(digest)
}

pub struct HerdrOutcome {
    digest: u64,
    wrong: Option<String>,
    stats: HerdrStats,
}

pub fn simulate(seed: u64, mutation: HerdrMutation) -> HerdrOutcome {
    mutation.install();
    let mut stats = HerdrStats {
        seeds: 1,
        ..HerdrStats::default()
    };
    let verdict = run(seed, &mut stats);
    HerdrMutation::None.install();
    let (digest, wrong) = match verdict {
        Ok(digest) => (digest, None),
        Err(message) => (0, Some(message)),
    };
    HerdrOutcome {
        digest,
        wrong,
        stats,
    }
}

pub fn judge(
    seed: u64,
    mutation: HerdrMutation,
    twice: bool,
    verbose: bool,
    totals: &mut HerdrStats,
) -> Option<String> {
    let first = simulate(seed, mutation);
    totals.add(&first.stats);
    if verbose {
        println!(
            "seed {seed} digest {:016x} stats={:?}",
            first.digest, first.stats
        );
    }
    if first.wrong.is_some() || !twice {
        return first.wrong;
    }
    let second = simulate(seed, mutation);
    (second.digest != first.digest || second.wrong.is_some()).then(|| {
        format!(
            "nondeterminism: digest {:016x} then {:016x}",
            first.digest, second.digest
        )
    })
}

pub fn replay(seed: u64, mutation: HerdrMutation) -> String {
    let mut command = format!("nanocode-sim --scenario herdr --seed {seed}");
    if mutation != HerdrMutation::None {
        command.push_str(" --mutation ");
        command.push_str(mutation.name());
    }
    command
}

pub fn report(
    seeds: &Range<u64>,
    passed: u64,
    failed: u64,
    twice: bool,
    mutation: HerdrMutation,
    totals: &HerdrStats,
) -> ExitCode {
    println!(
        "RESULT seeds={}..{} scenario=herdr mutation={} passed={passed} failed={failed} twice={twice} stats={totals:?}",
        seeds.start,
        seeds.end,
        mutation.name()
    );
    match mutation {
        HerdrMutation::None if failed == 0 => ExitCode::SUCCESS,
        HerdrMutation::None => ExitCode::FAILURE,
        _ if failed > 0 => {
            println!("MUTATION {} caught by {failed} seeds", mutation.name());
            ExitCode::SUCCESS
        }
        _ => {
            println!("MUTATION {} NOT caught", mutation.name());
            ExitCode::FAILURE
        }
    }
}
