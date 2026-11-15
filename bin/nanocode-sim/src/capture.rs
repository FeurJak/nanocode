use std::{ops::Range, process::ExitCode};

use nanocode_process::CapturedOutput;
use nanocode_process_contracts::{OutputUnit, Redactor};

use crate::rng::{FNV_OFFSET, Rng, fnv};

const SECRETS_MAX: u64 = 4;
const SECRET_LETTERS: u8 = 6;
const SECRET_BYTES_MIN: u64 = 6;
const SECRET_BYTES_MAX: u64 = 24;
const SECRET_PUNCTUATION_PPM: u64 = 150_000;
const SECRET_TRAILING_BACKSLASHES_PPM: u64 = 500_000;
const TAKES_MAX: u64 = 3;
const TAKE_BYTES_MAX: u64 = 3_000;
const SMALL_TAKE_PPM: u64 = 300_000;
const SMALL_TAKE_BYTES_MAX: u64 = 60;
const LIMIT_MAX: u64 = 700;
const ZERO_LIMIT_PPM: u64 = 30_000;
const CHUNK_BYTES_MAX: u64 = 400;
const TINY_CHUNK_PPM: u64 = 250_000;
const TINY_CHUNK_BYTES_MAX: u64 = 3;
const SECRET_PPM_MAX: u64 = 60_000;
const ESCAPED_PPM: u64 = 400_000;
const NEWLINE_PPM: u64 = 50_000;
const SUMMARY_BYTES_MAX: u64 = 8_000;
const SUMMARY_WINDOW_MAX: u64 = 3_000;
const SUMMARY_BOUND_MAX: u64 = 800;
const SUMMARY_WIDE_PPM_MAX: u64 = 20_000;
const SUMMARY_INVALID_PPM: u64 = 300_000;
const WIDE: [&str; 4] = ["\u{e9}", "\u{20ac}", "\u{1f600}", "\u{fffd}"];
const FILLER: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789 .,:;-_/=+()";
const PUNCTUATION: &[u8] = b"\"\\";
const MARKER_OPEN: &str = "\n... ";
const MARKER_CLOSE: &str = " bytes omitted ...\n";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureMutation {
    None,
    SkipTrim,
    DropChunk,
    SkipRedact,
    RedactThenTrim,
    NarrowKeep,
}

impl CaptureMutation {
    const ALL: [Self; 6] = [
        Self::None,
        Self::SkipTrim,
        Self::DropChunk,
        Self::SkipRedact,
        Self::RedactThenTrim,
        Self::NarrowKeep,
    ];

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|mutation| mutation.name() == name)
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::SkipTrim => "skip_trim",
            Self::DropChunk => "drop_chunk",
            Self::SkipRedact => "skip_redact",
            Self::RedactThenTrim => "redact_then_trim",
            Self::NarrowKeep => "narrow_keep",
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
}

#[derive(Clone, Copy, Debug, Default)]
pub struct CaptureStats {
    pub seeds: u64,
    pub takes: u64,
    pub bytes: u64,
    pub omitted: u64,
    pub secrets: u64,
    pub cut_secrets: u64,
}

impl CaptureStats {
    pub const fn add(&mut self, other: &Self) {
        self.seeds += other.seeds;
        self.takes += other.takes;
        self.bytes += other.bytes;
        self.omitted += other.omitted;
        self.secrets += other.secrets;
        self.cut_secrets += other.cut_secrets;
    }
}

struct Take {
    limit: usize,
    chunks: Vec<Vec<u8>>,
    secrets: u64,
}

struct Script {
    secrets: Vec<String>,
    takes: Vec<Take>,
}

fn wide(value: usize) -> u64 {
    u64::try_from(value).expect("a length fits in 64 bits")
}

fn narrow(value: u64) -> usize {
    usize::try_from(value).expect("a simulated size fits in usize")
}

fn secret_of(rng: &mut Rng, index: u8) -> String {
    let first = b'A' + index * SECRET_LETTERS;
    let length = rng.range(SECRET_BYTES_MIN, SECRET_BYTES_MAX);
    let trailing_backslashes = rng.chance(SECRET_TRAILING_BACKSLASHES_PPM);
    let mut secret = vec![first];
    while wide(secret.len()) < length {
        let byte = if !trailing_backslashes && rng.chance(SECRET_PUNCTUATION_PPM) {
            PUNCTUATION[rng.pick(PUNCTUATION.len())]
        } else {
            first
                + 1
                + u8::try_from(rng.below(u64::from(SECRET_LETTERS - 1)))
                    .expect("a letter offset fits in a byte")
        };
        secret.push(byte);
    }
    if trailing_backslashes {
        let length = secret.len();
        secret[length - 2..].copy_from_slice(b"\\\\");
    }
    String::from_utf8(secret).expect("a secret is ASCII")
}

impl Script {
    fn of(seed: u64) -> Self {
        let mut rng = Rng::derive(seed, "capture");
        let count = rng.below(SECRETS_MAX + 1);
        let secrets: Vec<String> = (0..count)
            .map(|index| secret_of(&mut rng, u8::try_from(index).expect("a few secrets")))
            .collect();
        let secret_ppm = rng.below(SECRET_PPM_MAX + 1);
        let takes = (0..rng.range(1, TAKES_MAX))
            .map(|_| {
                let bytes = if rng.chance(SMALL_TAKE_PPM) {
                    rng.below(SMALL_TAKE_BYTES_MAX + 1)
                } else {
                    rng.below(TAKE_BYTES_MAX + 1)
                };
                let limit = if rng.chance(ZERO_LIMIT_PPM) {
                    0
                } else {
                    rng.below(LIMIT_MAX + 1)
                };
                let mut stream = Vec::new();
                let mut inserted = 0;
                while wide(stream.len()) < bytes {
                    if !secrets.is_empty() && rng.chance(secret_ppm) {
                        let secret = &secrets[rng.pick(secrets.len())];
                        let form = if rng.chance(ESCAPED_PPM) {
                            nanocode_process_contracts::json_escape(secret)
                        } else {
                            secret.clone()
                        };
                        stream.extend_from_slice(form.as_bytes());
                        inserted += 1;
                    } else if rng.chance(NEWLINE_PPM) {
                        stream.push(b'\n');
                    } else {
                        stream.push(FILLER[rng.pick(FILLER.len())]);
                    }
                }
                let chunks = chunked(&mut rng, &stream);
                Take {
                    limit: narrow(limit),
                    chunks,
                    secrets: inserted,
                }
            })
            .collect();
        Self { secrets, takes }
    }

    fn describe(&self) -> String {
        let takes: Vec<String> = self
            .takes
            .iter()
            .map(|take| {
                format!(
                    "{}b/{}c/limit{}/{}s",
                    take.chunks.iter().map(Vec::len).sum::<usize>(),
                    take.chunks.len(),
                    take.limit,
                    take.secrets
                )
            })
            .collect();
        format!(
            "scenario=capture secrets={} takes=[{}]",
            self.secrets.len(),
            takes.join(" ")
        )
    }

    fn redactor(&self) -> Redactor {
        let mut redactor = Redactor::new(narrow(SECRET_BYTES_MIN));
        for (index, secret) in self.secrets.iter().enumerate() {
            redactor.add(secret, &format!("[S{index}]"));
        }
        redactor
    }
}

fn forms(redactor: &Redactor) -> Vec<String> {
    redactor.forms().map(|(form, _)| form.to_owned()).collect()
}

fn ends_with_partial(text: &str, forms: &[String]) -> Option<String> {
    forms.iter().find_map(|form| {
        (1..form.len())
            .find(|&length| text.ends_with(&form[..length]))
            .map(|length| form[..length].to_owned())
    })
}

fn starts_with_partial(text: &str, forms: &[String]) -> Option<String> {
    forms.iter().find_map(|form| {
        (1..form.len())
            .find(|&length| text.starts_with(&form[form.len() - length..]))
            .map(|length| form[form.len() - length..].to_owned())
    })
}

fn cuts_a_secret(full: &[u8], limit: usize, forms: &[String]) -> bool {
    let raw = String::from_utf8_lossy(full);
    let head_end = limit / 2;
    let tail_from = full.len() - (limit - head_end);
    forms.iter().any(|form| {
        raw.match_indices(form.as_str()).any(|(at, _)| {
            let end = at + form.len();
            (at < head_end && end > head_end) || (at < tail_from && end > tail_from)
        })
    })
}

fn omission_marker(rendered: &str) -> Option<(usize, usize, usize)> {
    rendered.match_indices(MARKER_OPEN).find_map(|(open, _)| {
        let after = &rendered[open + MARKER_OPEN.len()..];
        let close = after.find(MARKER_CLOSE)?;
        let count = &after[..close];
        (!count.is_empty() && count.bytes().all(|byte| byte.is_ascii_digit()))
            .then(|| count.parse().ok())
            .flatten()
            .map(|omitted| (open, close, omitted))
    })
}

fn partial_cut(text: &[u8], forms: &[String], at_end: bool) -> usize {
    forms
        .iter()
        .filter_map(|form| {
            let form = form.as_bytes();
            (1..=text.len().min(form.len().saturating_sub(1)))
                .rev()
                .find(|&length| {
                    if at_end {
                        text.ends_with(&form[..length])
                    } else {
                        text.starts_with(&form[form.len() - length..])
                    }
                })
        })
        .max()
        .unwrap_or(0)
}

fn redact_then_trim(raw: &[u8], omitted: usize, redactor: &Redactor, forms: &[String]) -> String {
    let text = String::from_utf8_lossy(raw).into_owned();
    let marker = format!("{MARKER_OPEN}{omitted}{MARKER_CLOSE}");
    let open = text
        .find(&marker)
        .expect("the raw output holds its omission marker");
    let mut head = text[..open].to_owned();
    let mut tail = text[open + marker.len()..].to_owned();
    redactor.text(&mut head);
    redactor.text(&mut tail);
    let head_cut = partial_cut(head.as_bytes(), forms, true);
    let tail_cut = partial_cut(tail.as_bytes(), forms, false);
    head.truncate(head.len() - head_cut);
    tail.drain(..tail_cut);
    let mut rendered = format!(
        "{head}{MARKER_OPEN}{}{MARKER_CLOSE}{tail}",
        omitted + head_cut + tail_cut
    );
    redactor.text(&mut rendered);
    rendered
}

fn check_take(
    index: usize,
    take: &Take,
    rendered: &str,
    redactor: &Redactor,
    forms: &[String],
) -> Result<bool, String> {
    let full: Vec<u8> = take.chunks.concat();
    let mut expected = String::from_utf8(full.clone()).expect("the generated output is ASCII");
    redactor.text(&mut expected);
    for form in forms {
        if rendered.contains(form.as_str()) {
            return Err(format!(
                "take {index}: a secret form {form:?} leaked into {rendered:?}"
            ));
        }
    }
    if full.len() <= take.limit {
        if rendered != expected {
            return Err(format!(
                "take {index}: {} bytes under limit {} rendered {rendered:?}, expected {expected:?}",
                full.len(),
                take.limit
            ));
        }
        return Ok(false);
    }
    let Some((open, close, omitted)) = omission_marker(rendered) else {
        return Err(format!(
            "take {index}: {} bytes over limit {} rendered no omission marker with a count: {rendered:?}",
            full.len(),
            take.limit
        ));
    };
    let after = &rendered[open + MARKER_OPEN.len()..];
    let head = &rendered[..open];
    let tail = &after[close + MARKER_CLOSE.len()..];
    if omitted < full.len() - take.limit || omitted > full.len() {
        return Err(format!(
            "take {index}: {omitted} bytes omitted of {} with limit {}",
            full.len(),
            take.limit
        ));
    }
    if !expected.starts_with(head) {
        return Err(format!(
            "take {index}: the kept head {head:?} does not begin the redacted output {expected:?}"
        ));
    }
    if !expected.ends_with(tail) {
        return Err(format!(
            "take {index}: the kept tail {tail:?} does not end the redacted output {expected:?}"
        ));
    }
    if let Some(partial) = ends_with_partial(head, forms) {
        return Err(format!(
            "take {index}: the kept head ends with {partial:?}, the start of a secret the omission cut"
        ));
    }
    if let Some(partial) = starts_with_partial(tail, forms) {
        return Err(format!(
            "take {index}: the kept tail starts with {partial:?}, the end of a secret the omission cut"
        ));
    }
    Ok(cuts_a_secret(&full, take.limit, forms))
}

pub struct CaptureOutcome {
    pub digest: u64,
    pub wrong: Option<String>,
    pub stats: CaptureStats,
    pub shape: String,
}

fn chunked(rng: &mut Rng, stream: &[u8]) -> Vec<Vec<u8>> {
    let mut chunks = Vec::new();
    let mut at = 0;
    while at < stream.len() {
        let size = if rng.chance(TINY_CHUNK_PPM) {
            rng.range(1, TINY_CHUNK_BYTES_MAX)
        } else {
            rng.range(1, CHUNK_BYTES_MAX)
        };
        let end = (at + narrow(size)).min(stream.len());
        chunks.push(stream[at..end].to_vec());
        at = end;
    }
    chunks
}

fn summarized(seed: u64, mutation: CaptureMutation) -> Option<String> {
    let mut rng = Rng::derive(seed, "capture-summary");
    let bound = narrow(rng.below(SUMMARY_BOUND_MAX + 1));
    let window = narrow(rng.below(SUMMARY_WINDOW_MAX + 1));
    let wide_ppm = rng.below(SUMMARY_WIDE_PPM_MAX + 1);
    let redactor = Redactor::new(narrow(SECRET_BYTES_MIN));
    let kept = match mutation {
        CaptureMutation::NarrowKeep => bound / 4,
        _ => bound,
    };
    let mut full = CapturedOutput::default();
    let mut summary = CapturedOutput::new(kept);
    for round in 0..rng.range(1, TAKES_MAX) {
        let bytes = rng.below(SUMMARY_BYTES_MAX + 1);
        let mut stream = Vec::new();
        while wide(stream.len()) < bytes {
            if !rng.chance(wide_ppm) {
                let filler = match rng.chance(NEWLINE_PPM) {
                    true => b'\n',
                    false => FILLER[rng.pick(FILLER.len())],
                };
                stream.push(filler);
            } else if rng.chance(SUMMARY_INVALID_PPM) {
                stream.push(0x80 | u8::try_from(rng.below(0x80)).expect("a byte"));
            } else {
                stream.extend_from_slice(WIDE[rng.pick(WIDE.len())].as_bytes());
            }
        }
        for chunk in chunked(&mut rng, &stream) {
            full.push(&chunk, window);
            summary.push(&chunk, window);
        }
        let (mut full, mut summary) = (full.take(), summary.take());
        if full.total_bytes() != summary.total_bytes() {
            return Some(format!(
                "summary round {round}: captured {} bytes, the full capture {}",
                summary.total_bytes(),
                full.total_bytes()
            ));
        }
        let original = full.total_bytes().saturating_add(3) / 4;
        for limit in [bound, narrow(rng.below(wide(bound) + 1))] {
            for unit in [OutputUnit::Tokens, OutputUnit::Chars] {
                let expected = full.limit(&redactor, limit, unit, original);
                let actual = summary.limit(&redactor, limit, unit, original);
                if expected != actual {
                    return Some(format!(
                        "summary round {round}: limit {limit} {unit:?} bound {bound} window {window} returned {} bytes, the full capture {}",
                        actual.len(),
                        expected.len()
                    ));
                }
            }
        }
    }
    None
}

fn simulate(seed: u64, mutation: CaptureMutation) -> CaptureOutcome {
    let script = Script::of(seed);
    let shape = script.describe();
    let redactor = script.redactor();
    let forms = forms(&redactor);
    let mut stats = CaptureStats {
        seeds: 1,
        secrets: wide(script.secrets.len()),
        ..CaptureStats::default()
    };
    let mut digest = fnv(FNV_OFFSET, shape.as_bytes());
    let mut captured = CapturedOutput::default();
    let mut wrong = None;
    let mut dropped = false;
    for (index, take) in script.takes.iter().enumerate() {
        for chunk in &take.chunks {
            if mutation == CaptureMutation::DropChunk && !dropped {
                dropped = true;
                continue;
            }
            captured.push(chunk, take.limit);
        }
        let mut chunk = captured.take();
        let rendered = match mutation {
            CaptureMutation::SkipTrim => {
                let mut raw = String::from_utf8_lossy(&chunk.with_omission_marker()).into_owned();
                redactor.text(&mut raw);
                raw
            }
            CaptureMutation::SkipRedact => {
                String::from_utf8_lossy(&chunk.with_omission_marker()).into_owned()
            }
            CaptureMutation::RedactThenTrim => match chunk
                .omission_marker()
                .and_then(|marker| marker.split(' ').nth(1)?.parse().ok())
            {
                Some(omitted) if !forms.is_empty() => {
                    redact_then_trim(&chunk.with_omission_marker(), omitted, &redactor, &forms)
                }
                _ => chunk.render(&redactor),
            },
            _ => chunk.render(&redactor),
        };
        let total = take.chunks.iter().map(Vec::len).sum::<usize>();
        stats.takes += 1;
        stats.bytes += wide(total);
        stats.omitted += u64::from(chunk.omission_marker().is_some());
        if chunk.total_bytes() != total && mutation != CaptureMutation::DropChunk {
            wrong = Some(format!(
                "take {index}: captured {} bytes of the {total} pushed",
                chunk.total_bytes()
            ));
            break;
        }
        digest = fnv(digest, rendered.as_bytes());
        match check_take(index, take, &rendered, &redactor, &forms) {
            Ok(cut) => stats.cut_secrets += u64::from(cut),
            Err(message) => {
                wrong = Some(message);
                break;
            }
        }
    }
    if wrong.is_none() {
        wrong = summarized(seed, mutation);
    }
    CaptureOutcome {
        digest,
        wrong,
        stats,
        shape,
    }
}

pub fn run_seed(seed: u64, mutation: CaptureMutation) -> CaptureOutcome {
    std::panic::catch_unwind(|| simulate(seed, mutation)).unwrap_or_else(|panic| {
        let message = panic
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| panic.downcast_ref::<&str>().map(|text| (*text).to_owned()))
            .unwrap_or_default();
        CaptureOutcome {
            digest: 0,
            wrong: Some(format!("panic: {message}")),
            stats: CaptureStats::default(),
            shape: String::new(),
        }
    })
}

pub fn judge(
    seed: u64,
    mutation: CaptureMutation,
    twice: bool,
    verbose: bool,
    totals: &mut CaptureStats,
) -> Option<String> {
    let first = run_seed(seed, mutation);
    totals.add(&first.stats);
    if verbose {
        println!("seed {seed} digest {:016x} {}", first.digest, first.shape);
    }
    if first.wrong.is_some() || !twice {
        return first
            .wrong
            .map(|message| format!("{} :: {message}", first.shape));
    }
    let second = run_seed(seed, mutation);
    (second.digest != first.digest || second.wrong.is_some()).then(|| {
        format!(
            "{} :: nondeterminism: trace digest {:016x} then {:016x}",
            first.shape, first.digest, second.digest
        )
    })
}

pub fn replay(seed: u64, mutation: CaptureMutation) -> String {
    let mut command = format!("nanocode-sim --scenario capture --seed {seed}");
    if mutation != CaptureMutation::None {
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
    mutation: CaptureMutation,
    totals: &CaptureStats,
) -> ExitCode {
    println!(
        "RESULT seeds={}..{} scenario=capture mutation={} passed={passed} failed={failed} twice={twice} stats={totals:?}",
        seeds.start,
        seeds.end,
        mutation.name()
    );
    match mutation {
        CaptureMutation::None if failed == 0 => ExitCode::SUCCESS,
        CaptureMutation::None => ExitCode::FAILURE,
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
