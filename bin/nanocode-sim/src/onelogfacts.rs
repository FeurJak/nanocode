use std::{collections::BTreeMap, ops::Range, process::ExitCode, sync::Arc};

use nanocode::turnlog::{EffectCall, LogHost, OneLog, Policy, facts_sim};
use nanocode_tool_runtime::ToolInput;
use nanocode_turnlog::EffectClass;
use nanocode_durability_core::{
    BlockRef, Decision, DurableFold, EffectResult, Fact, FactLog, FoldError, Folded, Input,
    InputKind, Intent, IntentTarget, Marker, ModelOut, Outcome as Finished, OwnedFact, Seen,
    StateStore, TurnStatus, VALUE_INLINE_BYTES, commit_fact, fold_facts,
};
use nanocode_sync::block_on;
use nanocode_turnlog::MemoryLog;
use serde::{Deserialize, Serialize};
use serde_json::value::{RawValue, to_raw_value};

use crate::{
    executor::Clock,
    rng::{FNV_OFFSET, Rng, fnv},
};

const INTENDED_INPUT: &str = "{\"tool_use\"";
const SCOPE: &str = "one_log_facts";
const TOOL: &str = "charge";
const STEPS: Range<u64> = 40..240;
const TURNS: u64 = 6;
const BLOCKS: u64 = 4;
const VALUES: u64 = 5;
const DECISIONS: u64 = 1 << 20;
const SNAPSHOT_RATIO: usize = 2;
const SNAPSHOT_ROWS_MIN: u64 = 2;
const SNAPSHOT_ROWS_MAX: u64 = 8;
const SWAP_PPM: u64 = 300_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OneLogFactsMutation {
    None,
    FoldReadsUnlogged,
    CommitBuffered,
    UnseenSkipped,
    IntentReplayed,
    IntentCopied,
}

impl OneLogFactsMutation {
    const ALL: [Self; 6] = [
        Self::None,
        Self::FoldReadsUnlogged,
        Self::CommitBuffered,
        Self::UnseenSkipped,
        Self::IntentReplayed,
        Self::IntentCopied,
    ];

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|mutation| mutation.name() == name)
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::FoldReadsUnlogged => "fold_reads_unlogged",
            Self::CommitBuffered => "commit_buffered",
            Self::UnseenSkipped => "unseen_skipped",
            Self::IntentReplayed => "intent_replayed",
            Self::IntentCopied => "intent_copied",
        }
    }

    pub fn names() -> String {
        Self::ALL[1..]
            .iter()
            .map(|mutation| mutation.name())
            .collect::<Vec<_>>()
            .join("|")
    }

    const fn log_rule(self) -> facts_sim::Mutation {
        match self {
            Self::CommitBuffered => facts_sim::Mutation::CommitBuffered,
            Self::UnseenSkipped => facts_sim::Mutation::UnseenSkipped,
            Self::IntentReplayed => facts_sim::Mutation::IntentReplayed,
            Self::IntentCopied => facts_sim::Mutation::IntentCopied,
            Self::None | Self::FoldReadsUnlogged => facts_sim::Mutation::None,
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct OneLogFactsStats {
    pub seeds: u64,
    pub facts: u64,
    pub blobs: u64,
    pub restarts: u64,
    pub refolds: u64,
    pub swaps: u64,
    pub unknown: u64,
    pub gated: u64,
}

impl OneLogFactsStats {
    pub const fn add(&mut self, other: &Self) {
        self.seeds += other.seeds;
        self.facts += other.facts;
        self.blobs += other.blobs;
        self.restarts += other.restarts;
        self.refolds += other.refolds;
        self.swaps += other.swaps;
        self.unknown += other.unknown;
        self.gated += other.gated;
    }
}

static UNLOGGED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static MUTATION: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

fn reads_unlogged() -> bool {
    MUTATION.load(std::sync::atomic::Ordering::Relaxed) == OneLogFactsMutation::FoldReadsUnlogged as u8
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct Ledger {
    digest: u64,
    turns: BTreeMap<String, u64>,
    finished: BTreeMap<String, String>,
    blocks: BTreeMap<u64, u32>,
    calls: BTreeMap<String, Option<u64>>,
    decisions: Vec<u64>,
    unlogged: u64,
}

impl Ledger {
    fn mix(&mut self, bytes: &[u8]) {
        self.digest = fnv(self.digest ^ FNV_OFFSET, bytes);
    }
}

impl DurableFold for Ledger {
    const FAMILY: &'static str = "sim.ledger";
    const FORMAT: u32 = 1;
    type Decision = u64;

    fn apply(&mut self, seq: u64, fact: Seen<'_, u64>) -> Result<(), FoldError> {
        match fact {
            Fact::Input(input) => {
                *self.turns.entry(input.turn).or_default() += 1;
                self.mix(input.value.get().as_bytes());
            }
            Fact::ModelOut(out) => {
                let blocks = u32::try_from(BLOCKS).expect("the block count fits in u32");
                self.blocks.insert(seq, blocks);
                self.mix(out.response.get().as_bytes());
            }
            Fact::Intent(intent) => {
                let IntentTarget::ToolUse(block) = intent.target else {
                    return Err(FoldError::Rejected {
                        seq,
                        reason: "the ledger takes only tool_use intents".to_owned(),
                    });
                };
                let held = self.blocks.get(&block.model_out).copied().unwrap_or(0);
                if block.block >= held {
                    return Err(FoldError::Rejected {
                        seq,
                        reason: format!(
                            "intent {} names block {} of model output {}, which has {held}",
                            intent.call_id, block.block, block.model_out
                        ),
                    });
                }
                self.calls.insert(intent.call_id, None);
            }
            Fact::EffectResult(result) => {
                let Some(slot) = self.calls.get_mut(&result.call_id) else {
                    return Err(FoldError::Rejected {
                        seq,
                        reason: format!("result of call {} without its intent", result.call_id),
                    });
                };
                *slot = Some(seq);
                self.mix(result.output.get().as_bytes());
            }
            Fact::Decision(Decision::Backend(chosen)) => self.decisions.push(*chosen),
            Fact::Marker(Marker::Status { turn, status }) => {
                self.finished.insert(turn, format!("{status:?}"));
            }
            Fact::Base(_) | Fact::Decision(_) | Fact::Marker(_) => {}
        }
        if reads_unlogged() {
            self.unlogged = UNLOGGED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        Ok(())
    }

    fn request(&self) -> Result<Vec<u8>, FoldError> {
        serde_json::to_vec(self).map_err(|error| FoldError::Encode(error.to_string()))
    }
}

struct World {
    host: Arc<MemoryLog>,
    log: Option<Arc<OneLog>>,
    facts: Option<Arc<dyn FactLog>>,
    live: Option<Folded<Ledger>>,
    stale: Option<Folded<Ledger>>,
    policy: Policy,
    outputs: Vec<u64>,
    open: Vec<String>,
    calls: u64,
    turns: u64,
    running: Vec<String>,
    stats: OneLogFactsStats,
    shape: String,
}

struct Outcome {
    digest: u64,
    wrong: Option<String>,
    stats: OneLogFactsStats,
    shape: String,
}

fn value(rng: &mut Rng) -> Box<RawValue> {
    let text = if rng.chance(200_000) {
        let pick = rng.below(VALUES);
        let bytes = usize::try_from(pick + 1).expect("a value index fits in usize") * VALUE_INLINE_BYTES;
        format!("{pick}:{}", "x".repeat(bytes))
    } else {
        format!("v{}", rng.below(DECISIONS))
    };
    to_raw_value(&text).expect("a string encodes")
}

impl World {
    fn facts(&self) -> &dyn FactLog {
        &**self.facts.as_ref().expect("the fact log is open")
    }

    fn open(&mut self) -> Result<(), String> {
        let host: Arc<dyn LogHost> = Arc::clone(&self.host) as Arc<dyn LogHost>;
        let log = block_on(OneLog::open(host, SCOPE, self.policy.clone()))
            .map_err(|error| format!("the facts log opens: {error}"))?;
        let facts = StateStore::facts(&log.store())
            .ok_or_else(|| "a OneLog store offers no fact log".to_owned())?;
        let live = block_on(fold_facts::<Ledger>(&*facts))
            .map_err(|error| format!("the facts log folds on open: {error}"))?;
        let stale = block_on(fold_facts::<Ledger>(&*facts))
            .map_err(|error| format!("the facts log folds on open: {error}"))?;
        self.stale = Some(stale);
        self.log = Some(log);
        self.facts = Some(facts);
        self.live = Some(live);
        Ok(())
    }

    fn fact(&mut self, rng: &mut Rng) -> OwnedFact<u64> {
        let turn = format!("turn-{}", rng.below(TURNS));
        match rng.below(100) {
            0..15 => {
                self.turns += 1;
                let turn = format!("admitted-{}", self.turns);
                self.running.push(turn.clone());
                Fact::Input(Input {
                    turn,
                    kind: InputKind::Prompt,
                    value: value(rng),
                    metadata: to_raw_value(&()).expect("null encodes"),
                    hand: None,
                })
            }
            15..40 => Fact::ModelOut(ModelOut {
                turn,
                response: value(rng),
            }),
            40..60 if !self.outputs.is_empty() => {
                self.calls += 1;
                let call_id = format!("call-{}", self.calls);
                self.open.push(call_id.clone());
                let model_out = self.outputs[rng.pick(self.outputs.len())];
                let block = u32::try_from(rng.below(BLOCKS)).expect("a block index fits in u32");
                Fact::Intent(Intent {
                    turn: Some(turn),
                    call_id,
                    tool: TOOL.to_owned(),
                    target: IntentTarget::ToolUse(BlockRef { model_out, block }),
                })
            }
            60..80 if !self.open.is_empty() => {
                let call_id = self.open.swap_remove(rng.pick(self.open.len()));
                Fact::EffectResult(EffectResult {
                    call_id,
                    output: value(rng),
                })
            }
            92.. if !self.running.is_empty() => Fact::Marker(Marker::Status {
                turn: self.running.swap_remove(rng.pick(self.running.len())),
                status: TurnStatus::Finished {
                    last: self.calls,
                    outcome: Finished::Completed,
                    result: to_raw_value(&()).expect("null encodes"),
                },
            }),
            _ => Fact::Decision(Decision::Backend(rng.below(DECISIONS))),
        }
    }

    fn commit(&mut self, rng: &mut Rng) -> Result<(), String> {
        let fact = self.fact(rng);
        let model_out = matches!(fact, Fact::ModelOut(_));
        let intent = match &fact {
            Fact::Intent(intent) => Some(intent.call_id.clone()),
            _ => None,
        };
        if rng.chance(SWAP_PPM) {
            self.stats.swaps += 1;
            std::mem::swap(&mut self.live, &mut self.stale);
        }
        let live = self.live.take().expect("the facts fold is live");
        let before = live.values().len();
        let (live, seq) = block_on(commit_fact(self.facts(), live, fact))
            .map_err(|error| format!("commit a fact: {error}"))?;
        self.stats.facts += 1;
        self.stats.blobs += live.values().len().saturating_sub(before) as u64;
        if model_out {
            self.outputs.push(seq);
        }
        if let Some(call_id) = intent {
            self.gate(&call_id)?;
        }
        self.shape.push('f');
        self.live = Some(live);
        self.check()
    }

    fn check(&mut self) -> Result<(), String> {
        self.stats.refolds += 1;
        let folded = block_on(fold_facts::<Ledger>(self.facts()))
            .map_err(|error| format!("refold the log: {error}"))?;
        let live = self.live.as_ref().expect("the facts fold is live");
        if folded.seq() != live.seq() {
            return Err(format!(
                "the log folds to fact {}, the live fold is at fact {}",
                folded.seq(),
                live.seq()
            ));
        }
        let encode = |state: &Ledger| serde_json::to_string(state).expect("a ledger encodes");
        if encode(folded.state()) != encode(live.state()) {
            return Err(format!(
                "the fold of the log differs from the live fold at fact {}",
                live.seq()
            ));
        }
        let requests = (folded.request(), live.request());
        let (Ok(again), Ok(now)) = requests else {
            return Err("a ledger request does not encode".to_owned());
        };
        if again != now {
            return Err(format!(
                "the next request folded from the log differs from the live one at fact {}",
                live.seq()
            ));
        }
        Ok(())
    }

    fn restart(&mut self) -> Result<(), String> {
        self.stats.restarts += 1;
        self.shape.push('!');
        let before = self.live.take().expect("the facts fold is live");
        self.facts = None;
        self.log = None;
        self.open()?;
        let after = self.live.as_ref().expect("the facts fold is live");
        let encode = |state: &Ledger| serde_json::to_string(state).expect("a ledger encodes");
        if after.seq() != before.seq() || encode(after.state()) != encode(before.state()) {
            return Err(format!(
                "after a restart the log folds to fact {}, not the fact {} the live fold had",
                after.seq(),
                before.seq()
            ));
        }
        Ok(())
    }

    fn gate(&mut self, call_id: &str) -> Result<(), String> {
        let log = Arc::clone(self.log.as_ref().expect("the facts log is open"));
        let input = ToolInput::Function(to_raw_value(&()).expect("null encodes"));
        block_on(log.gate().before(EffectCall {
            call_id,
            effect: EffectClass::External,
            input: &input,
            session_id: SCOPE,
            tool: TOOL,
            turn_id: None,
        }))
        .map_err(|error| format!("the gate refused intended call {call_id}: {error}"))?;
        let copied = block_on(log.read(|fold| {
            fold.open_calls()
                .any(|(id, call)| id == call_id && !call.input.get().starts_with(INTENDED_INPUT))
        }));
        if copied {
            return Err(format!(
                "the gate copied intended call {call_id} into a call record beside its intent"
            ));
        }
        self.stats.gated += 1;
        Ok(())
    }

    fn recover(&mut self) -> Result<(), String> {
        let log = Arc::clone(self.log.as_ref().expect("the facts log is open"));
        let mut settled = Vec::new();
        for turn in 0..TURNS {
            let turn = format!("turn-{turn}");
            settled.extend(
                block_on(log.settle_intents(&turn))
                    .map_err(|error| format!("settle the intents of {turn}: {error}"))?,
            );
        }
        settled.sort();
        let mut open = std::mem::take(&mut self.open);
        open.sort();
        if settled != open {
            return Err(format!(
                "recovery settled {settled:?} as outcome unknown, but {open:?} had no result"
            ));
        }
        self.stats.unknown += settled.len() as u64;
        let again = block_on(log.settle_intents("turn-0"))
            .map_err(|error| format!("settle again: {error}"))?;
        if !again.is_empty() {
            return Err(format!("recovery settled {again:?} twice"));
        }
        self.live = Some(
            block_on(fold_facts::<Ledger>(self.facts()))
                .map_err(|error| format!("refold after recovery: {error}"))?,
        );
        self.check()
    }

    fn step(&mut self, rng: &mut Rng) -> Result<(), String> {
        match rng.below(100) {
            0..90 => self.commit(rng),
            _ => {
                self.restart()?;
                self.recover()
            }
        }
    }
}

fn run_seed(seed: u64, mutation: OneLogFactsMutation) -> Outcome {
    facts_sim::mutate(mutation.log_rule());
    MUTATION.store(mutation as u8, std::sync::atomic::Ordering::SeqCst);
    crate::host::enter(seed, &Clock::new());
    let mut rng = Rng::derive(seed, "one_log_facts");
    let rows = usize::try_from(rng.range(SNAPSHOT_ROWS_MIN, SNAPSHOT_ROWS_MAX))
        .expect("a snapshot row count fits in usize");
    let mut world = World {
        host: Arc::new(MemoryLog::new()),
        log: None,
        facts: None,
        live: None,
        stale: None,
        policy: Policy {
            snapshot_ratio: SNAPSHOT_RATIO,
            snapshot_rows: Some(rows),
            ..Policy::default()
        },
        outputs: Vec::new(),
        turns: 0,
        running: Vec::new(),
        open: Vec::new(),
        calls: 0,
        stats: OneLogFactsStats {
            seeds: 1,
            ..OneLogFactsStats::default()
        },
        shape: String::new(),
    };
    let steps = rng.range(STEPS.start, STEPS.end);
    let mut wrong = world.open().err();
    for step in 0..steps {
        if wrong.is_some() {
            break;
        }
        wrong = world
            .step(&mut rng)
            .err()
            .map(|message| format!("step {step}: {message}"));
    }
    facts_sim::mutate(facts_sim::Mutation::None);
    MUTATION.store(OneLogFactsMutation::None as u8, std::sync::atomic::Ordering::SeqCst);
    Outcome {
        digest: fnv(FNV_OFFSET, world.shape.as_bytes()),
        wrong,
        stats: world.stats,
        shape: format!("steps={steps} snapshot_rows={rows} ops={}", world.shape),
    }
}

pub fn judge(
    seed: u64,
    mutation: OneLogFactsMutation,
    twice: bool,
    verbose: bool,
    totals: &mut OneLogFactsStats,
) -> Option<String> {
    let first = run_seed(seed, mutation);
    totals.add(&first.stats);
    if verbose {
        println!(
            "seed {seed} digest {:016x} {} stats={:?}",
            first.digest, first.shape, first.stats
        );
    }
    if first.wrong.is_some() || !twice {
        return first
            .wrong
            .map(|message| format!("{} :: {message}", first.shape));
    }
    let second = run_seed(seed, mutation);
    (second.digest != first.digest || second.wrong.is_some()).then(|| {
        format!(
            "{} :: nondeterminism: digest {:016x} then {:016x}",
            first.shape, first.digest, second.digest
        )
    })
}

pub fn replay(seed: u64, mutation: OneLogFactsMutation) -> String {
    let mut command = format!("nanocode-sim --scenario one_log_facts --seed {seed}");
    if mutation != OneLogFactsMutation::None {
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
    mutation: OneLogFactsMutation,
    totals: &OneLogFactsStats,
) -> ExitCode {
    println!(
        "RESULT seeds={}..{} scenario=one_log_facts mutation={} passed={passed} failed={failed} twice={twice} stats={totals:?}",
        seeds.start,
        seeds.end,
        mutation.name()
    );
    match mutation {
        OneLogFactsMutation::None if failed == 0 => ExitCode::SUCCESS,
        OneLogFactsMutation::None => ExitCode::FAILURE,
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
