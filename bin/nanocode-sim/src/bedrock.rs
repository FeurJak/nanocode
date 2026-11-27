use std::{ops::Range, process::ExitCode, sync::Arc};

use nanocode::claude::ClaudeClient;
use nanocode::claude::bedrock::{Bedrock, BedrockConfig, Lookup, REGION_PREFIXES};
use nanocode_aws::sim::Mutation as AwsRule;
use nanocode_backend_claude_client::sim::Mutation as ClaudeRule;
use nanocode_http::layer::{BoxFuture, Error, Layer, Next, Request, Response, Sender};
use nanocode_sync::{Atom, StreamExt};
use serde_json::{Value, json};

use crate::executor::{Clock, Executor, Task};
use crate::rng::{FNV_OFFSET, Rng, fnv};

mod wire;

const EVENTS_MIN: u64 = 2;
const EVENTS_MAX: u64 = 12;
const TEXT_MAX: u64 = 24;
const CHUNK_MAX: u64 = 160;
const PIN_PPM: u64 = 300_000;
const ARN_PPM: u64 = 400_000;
const PREFIX_PPM: u64 = 400_000;
const MODELS: [&str; 3] = ["claude-opus-5-5", "claude-sonnet-5-5", "claude-haiku-5-5"];
const PINS: [&str; 3] = [
    "ANTHROPIC_DEFAULT_OPUS_MODEL",
    "ANTHROPIC_DEFAULT_SONNET_MODEL",
    "ANTHROPIC_DEFAULT_HAIKU_MODEL",
];
const BASE: &str = "https://bedrock-runtime.sim-1.amazonaws.com";
const KEY_ID: &str = "AKIDSIMULATOR";
const SECRET: &str = "sim/secret+key";
const TOKEN: &str = "sim-session-token";
const BETAS: [&str; 2] = ["context-management-2025-06-27", "sim-beta-2026-01-01"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BedrockMutation {
    None,
    SkipsMessageCrc,
    SkipsPreludeCrc,
    SingleEncodedPath,
    ExceptionIgnored,
    FallbackUnchecked,
}

impl BedrockMutation {
    const ALL: [Self; 6] = [
        Self::None,
        Self::SkipsMessageCrc,
        Self::SkipsPreludeCrc,
        Self::SingleEncodedPath,
        Self::ExceptionIgnored,
        Self::FallbackUnchecked,
    ];

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|mutation| mutation.name() == name)
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::SkipsMessageCrc => "skips_message_crc",
            Self::SkipsPreludeCrc => "skips_prelude_crc",
            Self::SingleEncodedPath => "single_encoded_path",
            Self::ExceptionIgnored => "exception_ignored",
            Self::FallbackUnchecked => "fallback_unchecked",
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
        nanocode_aws::sim::mutate(match self {
            Self::SkipsMessageCrc => AwsRule::SkipsMessageCrc,
            Self::SkipsPreludeCrc => AwsRule::SkipsPreludeCrc,
            Self::SingleEncodedPath => AwsRule::SingleEncodedPath,
            _ => AwsRule::None,
        });
        nanocode_backend_claude_client::sim::mutate(match self {
            Self::ExceptionIgnored => ClaudeRule::ExceptionIgnored,
            Self::FallbackUnchecked => ClaudeRule::FallbackUnchecked,
            _ => ClaudeRule::None,
        });
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct BedrockStats {
    pub seeds: u64,
    pub events: u64,
    pub chunks: u64,
    pub split_frames: u64,
    pub garbled: u64,
    pub exceptions: u64,
    pub truncated: u64,
    pub encoded_paths: u64,
    pub fallbacks: u64,
}

impl BedrockStats {
    pub const fn add(&mut self, other: &Self) {
        self.seeds += other.seeds;
        self.events += other.events;
        self.chunks += other.chunks;
        self.split_frames += other.split_frames;
        self.garbled += other.garbled;
        self.exceptions += other.exceptions;
        self.truncated += other.truncated;
        self.encoded_paths += other.encoded_paths;
        self.fallbacks += other.fallbacks;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Fault {
    None,
    Header { frame: usize },
    PreludeCrc { frame: usize },
    Exception { frame: usize },
    Truncated { frame: usize },
}

struct Plan {
    env: Vec<(&'static str, String)>,
    model: &'static str,
    events: Vec<Value>,
    fault: Fault,
    chunk_max: u64,
}

fn plan(rng: &mut Rng) -> Plan {
    let mut env = vec![
        ("AWS_ACCESS_KEY_ID", KEY_ID.to_owned()),
        ("AWS_SECRET_ACCESS_KEY", SECRET.to_owned()),
        ("AWS_SESSION_TOKEN", TOKEN.to_owned()),
        ("AWS_REGION", "sim-1".to_owned()),
        ("ANTHROPIC_BEDROCK_BASE_URL", BASE.to_owned()),
    ];
    if rng.chance(PREFIX_PPM) {
        let prefix = REGION_PREFIXES[rng.pick(REGION_PREFIXES.len())];
        env.push(("ANTHROPIC_BEDROCK_REGION_PREFIX", prefix.to_owned()));
    }
    for (family, name) in PINS.iter().enumerate() {
        if rng.chance(PIN_PPM) {
            let id = match rng.chance(ARN_PPM) {
                true => format!(
                    "arn:aws:bedrock:sim-1:000000000000:application-inference-profile/sim{family}"
                ),
                false => format!("us.anthropic.{}-v{}:0", MODELS[family], rng.range(1, 3)),
            };
            env.push((name, id));
        }
    }
    let model = MODELS[rng.pick(MODELS.len())];
    let count = rng.range(EVENTS_MIN, EVENTS_MAX) as usize;
    let mut events = vec![
        json!({"type": "message_start", "message": {"id": "msg_sim", "type": "message", "role": "assistant", "model": model, "content": [], "usage": {"input_tokens": 3, "output_tokens": 0}}}),
    ];
    for index in 1..count.saturating_sub(1) {
        let text = (0..rng.range(1, TEXT_MAX))
            .map(|_| char::from(b'a' + u8::try_from(rng.below(26)).unwrap_or(0)))
            .collect::<String>();
        events.push(json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": format!("{index}:{text}")}}));
    }
    events.push(json!({"type": "message_stop"}));
    let frame = rng.pick(events.len());
    let fault = match rng.below(5) {
        0 => Fault::Header { frame },
        1 => Fault::PreludeCrc { frame },
        2 => Fault::Exception { frame },
        3 => Fault::Truncated { frame },
        _ => Fault::None,
    };
    Plan {
        env,
        model,
        events,
        fault,
        chunk_max: rng.range(1, CHUNK_MAX),
    }
}

struct Mock {
    chunks: Vec<Vec<u8>>,
    seen: Atom<Option<Request>>,
}

impl Sender for Mock {
    fn send(&self, request: Request) -> BoxFuture<'_, Result<Response, Error>> {
        self.seen.set(Some(request));
        let chunks = self.chunks.clone().into_iter().map(Ok).collect::<Vec<_>>();
        Box::pin(async move {
            let mut headers = nanocode_http::types::HeaderMap::new();
            headers.insert(
                nanocode_http::types::header::CONTENT_TYPE,
                nanocode_http::types::HeaderValue::from_static(
                    "application/vnd.amazon.eventstream",
                ),
            );
            Ok(Response::new(
                nanocode_http::types::StatusCode::OK,
                headers,
                Box::pin(nanocode_sync::stream::iter(chunks)),
            ))
        })
    }
}

fn lookup_of(env: &[(&'static str, String)]) -> Lookup {
    let env = env.to_vec();
    Arc::new(move |name: &str| {
        env.iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value.clone())
    })
}

struct Body {
    chunks: Vec<Vec<u8>>,
    delivered: usize,
    split: u64,
}

fn body(plan: &Plan, rng: &mut Rng) -> Body {
    let mut stream = Vec::new();
    let mut bounds = Vec::new();
    let mut delivered = plan.events.len();
    for (index, event) in plan.events.iter().enumerate() {
        let mut frame = match plan.fault {
            Fault::Exception { frame } if frame == index => wire::exception(),
            _ => wire::chunk(event),
        };
        match plan.fault {
            Fault::Header { frame: at } if at == index => {
                frame.bytes[frame.event_type_at] ^= 0x01;
                delivered = index;
            }
            Fault::PreludeCrc { frame: at } if at == index => {
                frame.bytes[8] ^= 0x01;
                let crc_at = frame.bytes.len() - 4;
                let message = wire::crc32(&frame.bytes[..crc_at]);
                frame.bytes[crc_at..].copy_from_slice(&message.to_be_bytes());
                delivered = index;
            }
            Fault::Exception { frame: at } | Fault::Truncated { frame: at } if at == index => {
                delivered = index;
            }
            _ => {}
        }
        let end = stream.len() + frame.bytes.len();
        bounds.push(end);
        stream.extend_from_slice(&frame.bytes);
        if matches!(plan.fault, Fault::Exception { frame } if frame == index) {
            break;
        }
        if let Fault::Truncated { frame: at } = plan.fault
            && at == index
        {
            let cut = 1 + rng.below(frame.bytes.len() as u64 - 1) as usize;
            stream.truncate(end - cut);
            break;
        }
    }
    let mut chunks = Vec::new();
    let mut at = 0;
    let mut split = 0;
    while at < stream.len() {
        let size = (rng.range(1, plan.chunk_max) as usize).min(stream.len() - at);
        if !bounds.contains(&(at + size)) {
            split += 1;
        }
        chunks.push(stream[at..at + size].to_vec());
        at += size;
    }
    Body {
        chunks,
        delivered,
        split,
    }
}

fn request(model: &str) -> Request {
    let mut headers = nanocode_http::types::HeaderMap::new();
    headers.insert(
        "anthropic-beta",
        nanocode_http::types::HeaderValue::from_str(&BETAS.join(","))
            .unwrap_or_else(|_| nanocode_http::types::HeaderValue::from_static("invalid")),
    );
    headers.insert(
        "x-api-key",
        nanocode_http::types::HeaderValue::from_static("must-not-leak"),
    );
    let body = json!({"model": model, "stream": true, "max_tokens": 8, "messages": [{"role": "user", "content": "hi"}]});
    Request {
        method: nanocode_http::Method::POST,
        url: "https://sim.invalid/v1/messages".to_owned(),
        headers,
        body: body.to_string().into_bytes().into(),
    }
}

fn check_request(sent: &Request, id: &str, stats: &mut BedrockStats) -> Result<(), String> {
    let path = format!(
        "{BASE}/model/{}/invoke-with-response-stream",
        wire::encode(id).replace('/', "%2F")
    );
    if sent.url != path {
        return Err(format!("the request went to {}, not {path}", sent.url));
    }
    if id.contains([':', '/']) {
        stats.encoded_paths += 1;
    }
    if sent.headers.contains_key("x-api-key") || sent.headers.contains_key("anthropic-beta") {
        return Err("the Bedrock request kept x-api-key or anthropic-beta".to_owned());
    }
    let body: Value = serde_json::from_slice(&sent.body)
        .map_err(|error| format!("the Bedrock body is not JSON: {error}"))?;
    let first = body
        .as_object()
        .and_then(|fields| fields.keys().next().cloned());
    if first.as_deref() != Some("anthropic_version")
        || body["anthropic_version"] != "bedrock-2023-05-31"
        || body.get("model").is_some()
        || body.get("stream").is_some()
        || body["anthropic_beta"] != json!(BETAS)
    {
        return Err(format!("the Bedrock body is not the invoke form: {body}"));
    }
    wire::verify(sent, SECRET)
}

fn check_stream(plan: &Plan, text: &str, failed: bool, delivered: usize) -> Result<(), String> {
    let events = wire::sse(text)?;
    let errors = events.iter().filter(|(name, _)| name == "error").count();
    let data = events
        .iter()
        .filter(|(name, _)| name != "error")
        .map(|(_, data)| data.clone())
        .collect::<Vec<_>>();
    if data != plan.events[..delivered] {
        return Err(format!(
            "the SSE stream carried {} events where the frames before the fault ({:?}) hold {delivered}",
            data.len(),
            plan.fault
        ));
    }
    match plan.fault {
        Fault::None if failed || errors > 0 => {
            Err("an intact event stream ended with an error".to_owned())
        }
        Fault::Exception { .. } if failed || errors != 1 => Err(format!(
            "an exception frame became {errors} SSE error events and transport failure {failed}"
        )),
        Fault::Exception { .. } => match &events.last() {
            Some((_, error)) if error["error"]["type"] == "rate_limit_error" => Ok(()),
            other => Err(format!("a throttling exception became {other:?}")),
        },
        Fault::Header { .. } | Fault::PreludeCrc { .. } | Fault::Truncated { .. } if !failed => {
            Err(format!(
                "a corrupt event stream ({:?}) ended without an error",
                plan.fault
            ))
        }
        _ => Ok(()),
    }
}

fn check_fallbacks(plan: &Plan, stats: &mut BedrockStats) -> Result<(), String> {
    let lookup = lookup_of(&plan.env);
    let config = BedrockConfig::from_lookup(lookup.as_ref())
        .map_err(|error| format!("the Bedrock config was refused: {error}"))?;
    let listed = config.refusal_fallbacks();
    stats.fallbacks += listed.len() as u64;
    let client = ClaudeClient::bedrock(
        ClaudeClient::http_client(),
        Bedrock::new(
            config.clone(),
            lookup,
            Arc::new(Mock {
                chunks: Vec::new(),
                seen: Atom::new(None),
            }),
        ),
    );
    for fallback in ["claude-opus-5", "claude-opus-4-8", "claude-sonnet-5"] {
        let served = client.serves_refusal_fallback(fallback);
        if served != listed.iter().any(|listed| listed == fallback) {
            return Err(format!(
                "the client serves {fallback}: {served}, against the config's list {listed:?}"
            ));
        }
        let global = config.model_id(fallback) == format!("global.anthropic.{fallback}");
        let family = PINS
            .iter()
            .zip(MODELS)
            .find(|(_, model)| model.split('-').nth(1) == fallback.split('-').nth(1));
        let pinned = family.is_some_and(|(name, _)| plan.env.iter().any(|(key, _)| key == name));
        if served && (!global || pinned) {
            return Err(format!(
                "a refusal may fall back to {fallback}, sent as {}, which is not the served global model (pinned: {pinned})",
                config.model_id(fallback)
            ));
        }
    }
    Ok(())
}

async fn exercise(plan: &Plan, body: Body, stats: &mut BedrockStats) -> Result<u64, String> {
    let lookup = lookup_of(&plan.env);
    let config = BedrockConfig::from_lookup(lookup.as_ref())
        .map_err(|error| format!("the Bedrock config was refused: {error}"))?;
    let id = config.model_id(plan.model);
    let mock = Mock {
        chunks: body.chunks,
        seen: Atom::new(None),
    };
    let layer = Bedrock::new(
        config,
        lookup,
        Arc::new(Mock {
            chunks: Vec::new(),
            seen: Atom::new(None),
        }),
    );
    let response = layer
        .send(request(plan.model), Next::new(&mock))
        .await
        .map_err(|error| format!("the Bedrock layer failed: {}", error.detail()))?;
    let sent = mock.seen.get();
    let Some(sent) = sent.as_ref() else {
        return Err("the Bedrock layer sent nothing".to_owned());
    };
    check_request(sent, &id, stats)?;
    let mut stream = response.body;
    let mut text = Vec::new();
    let mut failed = false;
    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(bytes) => text.extend_from_slice(&bytes),
            Err(_) => {
                failed = true;
                break;
            }
        }
    }
    let text =
        String::from_utf8(text).map_err(|error| format!("the SSE stream is not UTF-8: {error}"))?;
    check_stream(plan, &text, failed, body.delivered)?;
    Ok(fnv(FNV_OFFSET, text.as_bytes()))
}

fn run(seed: u64, stats: &mut BedrockStats) -> Result<u64, String> {
    let mut rng = Rng::derive(seed, "bedrock");
    let plan = plan(&mut rng);
    let body = body(&plan, &mut rng);
    stats.events = plan.events.len() as u64;
    stats.chunks = body.chunks.len() as u64;
    stats.split_frames = body.split;
    match plan.fault {
        Fault::Header { .. } | Fault::PreludeCrc { .. } => stats.garbled = 1,
        Fault::Exception { .. } => stats.exceptions = 1,
        Fault::Truncated { .. } => stats.truncated = 1,
        Fault::None => {}
    }
    let clock = Clock::new();
    crate::host::entered(seed, &clock, || {
        check_fallbacks(&plan, stats)?;
        let result = std::cell::RefCell::new(None);
        let mut executor = Executor::new(Arc::clone(&clock), Rng::derive(seed, "scheduler"));
        let task: Task<'_> = Box::pin(async {
            result.replace(Some(exercise(&plan, body, stats).await));
        });
        let ended = executor.run(vec![task], None);
        result.into_inner().unwrap_or_else(|| {
            Err(format!(
                "the Bedrock exchange ended {ended:?} without a verdict"
            ))
        })
    })
}

pub struct BedrockOutcome {
    digest: u64,
    wrong: Option<String>,
    stats: BedrockStats,
}

pub fn simulate(seed: u64, mutation: BedrockMutation) -> BedrockOutcome {
    mutation.install();
    let mut stats = BedrockStats {
        seeds: 1,
        ..BedrockStats::default()
    };
    let verdict = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(seed, &mut stats)))
        .unwrap_or_else(|_| Err("panic".to_owned()));
    BedrockMutation::None.install();
    let (digest, wrong) = match verdict {
        Ok(digest) => (digest, None),
        Err(message) => (0, Some(message)),
    };
    BedrockOutcome {
        digest,
        wrong,
        stats,
    }
}

pub fn judge(
    seed: u64,
    mutation: BedrockMutation,
    twice: bool,
    verbose: bool,
    totals: &mut BedrockStats,
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

pub fn replay(seed: u64, mutation: BedrockMutation) -> String {
    let mut command = format!("nanocode-sim --scenario bedrock --seed {seed}");
    if mutation != BedrockMutation::None {
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
    mutation: BedrockMutation,
    totals: &BedrockStats,
) -> ExitCode {
    println!(
        "RESULT seeds={}..{} scenario=bedrock mutation={} passed={passed} failed={failed} twice={twice} stats={totals:?}",
        seeds.start,
        seeds.end,
        mutation.name()
    );
    match mutation {
        BedrockMutation::None if failed == 0 => ExitCode::SUCCESS,
        BedrockMutation::None => ExitCode::FAILURE,
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
