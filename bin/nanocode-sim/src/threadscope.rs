use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    num::NonZeroU32,
    ops::Range,
    process::ExitCode,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    task::{Context, Poll, Waker},
};

use nanocode::{
    backend::Prompt,
    durability::facts::{Decision, EffectResult, Fact, Intent, IntentTarget},
    session_host::{LogEntry, Submission, TurnState},
    turnlog::{
        ThreadCommand, ThreadFacts, ThreadFold, ThreadLog, ThreadLogs, ThreadPolicy, ThreadReply,
        ThreadState,
        thread_scope_sim::{self, Mutation},
    },
};
use nanocode_durability_core::{
    DurableFold, FactLogs, FoldError, Input, InputKind, OwnedFact, Seen, StoreError, StoreFuture,
    VALUE_INLINE_BYTES, commit_fact, fold_facts,
};
use nanocode_scope::{
    Checkpoints, Committed, Manifest, MemoryCheckpoints, Part, PartedLeases, Policy, Resident,
    Route, ScopeError, Wiring,
    sim::{self as scope_sim, Mutation as ScopeMutation},
};
use nanocode_sync::{Atom, block_on};
use nanocode_turnlog::{
    Fold, Hold, LogHost, Phase, Record, SCAN_END, Wake,
    parted::{ModelParts, PartStore, PartedLog},
};
use serde::{Deserialize, Serialize};
use serde_json::{Map, json, value::RawValue};

use crate::{
    executor::Clock,
    rng::{FNV_OFFSET, Rng, fnv},
    threadlegacy::{self, Legacy, View},
};

pub(crate) const PARTS: u32 = 2;
const THREADS: Range<u64> = 2..5;
const NODES: Range<u64> = 2..4;
const STEPS: Range<u64> = 120..320;
const TTL_MS: u64 = 1_000;
const GRACE_MS: u64 = 300;
const MARGIN_MS: u64 = 100;
const TICK_MS_MAX: u64 = 250;
const START_MS: u64 = 1_000_000;
const CHECKPOINT_ROWS: Range<u64> = 2..8;
const CHECKPOINT_MS: u64 = 1_500;
const RESIDENT_BYTES: Range<u64> = 1_500..12_000;
const PART_ROWS: Range<u64> = 1..5;
const FORWARD_MS: u64 = 50;
const SESSIONS: u64 = 2;
const CELLS: u64 = 3;
const RELEASE_PPM: u64 = 40_000;
const CRASH_PPM: u64 = 12_000;
const TICK_PPM: u64 = 120_000;
const LEGACY_PPM: u64 = 500_000;
const JOB_PPM: u64 = 150_000;
const FACT_LOG_PPM: u64 = 100_000;
const FACT_BLOB_PPM: u64 = 300_000;
const FACT_STATES: u64 = 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ThreadScopeMutation(Mutation, ScopeMutation);

const SCOPE_MUTATIONS: [ScopeMutation; 2] = [
    ScopeMutation::ListingSkipsDropped,
    ScopeMutation::AdoptionSkipsCheckpoint,
];

impl ThreadScopeMutation {
    pub const NONE: Self = Self(Mutation::None, ScopeMutation::None);

    pub fn parse(name: &str) -> Option<Self> {
        if let Some(mutation) = SCOPE_MUTATIONS.into_iter().find(|scope| scope.name() == name) {
            return Some(Self(Mutation::None, mutation));
        }
        Mutation::parse(name).map(|mutation| Self(mutation, ScopeMutation::None))
    }

    pub const fn name(self) -> &'static str {
        match self.1 {
            ScopeMutation::None => self.0.name(),
            scope => scope.name(),
        }
    }

    pub fn names() -> String {
        Mutation::ALL
            .iter()
            .map(|mutation| mutation.name())
            .chain(SCOPE_MUTATIONS.iter().map(|mutation| mutation.name()))
            .collect::<Vec<_>>()
            .join("|")
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ThreadScopeStats {
    pub turn_ops: u64,
    pub turn_reopens: u64,
    pub turn_lenient: u64,
    pub hold_ops: u64,
    pub hold_reopens: u64,
    pub hold_settles: u64,
    pub wake_ops: u64,
    pub seeds: u64,
    pub steps: u64,
    pub sends: u64,
    pub committed: u64,
    pub refused: u64,
    pub releases: u64,
    pub crashes: u64,
    pub resident_checks: u64,
    pub cold_checks: u64,
    pub legacy_checks: u64,
    pub legacy_threads: u64,
    pub legacy_turns: u64,
    pub legacy_retired: u64,
    pub legacy_blobs: u64,
    pub legacy_paged_rows: u64,
    pub adopted_checks: u64,
    pub adoptions: u64,
    pub adoption_rows: u64,
    pub loads: u64,
    pub load_rows_max: u64,
    pub checkpoints: u64,
    pub checkpoint_parts: u64,
    pub checkpoint_parts_kept: u64,
    pub evictions: u64,
    pub forwards: u64,
    pub jobs: u64,
    pub jobs_routed: u64,
    pub fact_logs: u64,
    pub fact_logs_routed: u64,
    pub fact_commits: u64,
}

impl ThreadScopeStats {
    pub fn add(&mut self, other: &Self) {
        self.turn_ops += other.turn_ops;
        self.turn_reopens += other.turn_reopens;
        self.turn_lenient += other.turn_lenient;
        self.hold_ops += other.hold_ops;
        self.hold_reopens += other.hold_reopens;
        self.hold_settles += other.hold_settles;
        self.wake_ops += other.wake_ops;
        self.seeds += other.seeds;
        self.steps += other.steps;
        self.sends += other.sends;
        self.committed += other.committed;
        self.refused += other.refused;
        self.releases += other.releases;
        self.crashes += other.crashes;
        self.resident_checks += other.resident_checks;
        self.cold_checks += other.cold_checks;
        self.legacy_checks += other.legacy_checks;
        self.legacy_threads += other.legacy_threads;
        self.legacy_turns += other.legacy_turns;
        self.legacy_retired += other.legacy_retired;
        self.legacy_blobs += other.legacy_blobs;
        self.legacy_paged_rows += other.legacy_paged_rows;
        self.adopted_checks += other.adopted_checks;
        self.adoptions += other.adoptions;
        self.adoption_rows += other.adoption_rows;
        self.loads += other.loads;
        self.load_rows_max = self.load_rows_max.max(other.load_rows_max);
        self.checkpoints += other.checkpoints;
        self.checkpoint_parts += other.checkpoint_parts;
        self.checkpoint_parts_kept += other.checkpoint_parts_kept;
        self.evictions += other.evictions;
        self.forwards += other.forwards;
        self.jobs += other.jobs;
        self.jobs_routed += other.jobs_routed;
        self.fact_logs += other.fact_logs;
        self.fact_logs_routed += other.fact_logs_routed;
        self.fact_commits += other.fact_commits;
    }
}

type Wrong = Arc<Atom<Option<String>>>;

pub(crate) fn flag(wrong: &Wrong, message: String) {
    wrong.update(|wrong| {
        if wrong.is_none() {
            *wrong = Some(message.clone());
        }
    });
}

#[derive(Default)]
struct Net {
    nodes: Atom<Vec<(String, ThreadLogs)>>,
}

impl Route<ThreadFold> for Net {
    fn forward<'a>(
        &'a self,
        holder: &'a str,
        scope: &'a str,
        command: ThreadCommand,
    ) -> StoreFuture<'a, Result<Committed<ThreadReply>, ScopeError>> {
        Box::pin(async move {
            let target = self.nodes.read(|nodes| {
                nodes
                    .iter()
                    .find(|(name, _)| name == holder)
                    .map(|(_, logs)| logs.clone())
            });
            match target {
                Some(logs) => logs.scopes().serve(scope, command).await,
                None => Err(ScopeError::Unreachable {
                    holder: holder.to_owned(),
                }),
            }
        })
    }
}

pub(crate) struct Node {
    name: String,
    pub(crate) logs: ThreadLogs,
    handles: BTreeMap<String, ThreadLog>,
}

pub(crate) struct World {
    time: Arc<Clock>,
    db: Arc<AtomicU64>,
    store: Arc<ModelParts>,
    witness: Arc<MemoryCheckpoints<ModelParts>>,
    net: Arc<Net>,
    reader: PartedLog<ModelParts>,
    fold: ThreadFold,
    thread: ThreadPolicy,
    policy: Policy,
    count: NonZeroU32,
    threads: Vec<String>,
    legacy: BTreeMap<String, (Legacy, View)>,
    bases: RefCell<BTreeMap<String, (u64, ThreadState)>>,
    gone: Cell<(u64, u64)>,
    pub(crate) wrong: Wrong,
}

impl World {
    pub(crate) fn node(&self, name: &str) -> Node {
        let db = Arc::clone(&self.db);
        let log = Arc::new(
            PartedLog::fenced(Arc::clone(&self.store), self.count, name)
                .with_clock(Arc::new(move || db.load(Ordering::Relaxed)))
                .with_lease_bounds(GRACE_MS, MARGIN_MS),
        );
        let leases = PartedLeases::new(Arc::clone(&log), name, TTL_MS);
        let db = Arc::clone(&self.db);
        let logs = ThreadLogs::new(Wiring {
            fold: Arc::new(ThreadFold::new(self.thread)),
            log: log as Arc<dyn LogHost>,
            leases: Arc::new(leases),
            checkpoints: Arc::clone(&self.witness) as Arc<dyn Checkpoints>,
            route: Arc::clone(&self.net) as Arc<dyn Route<ThreadFold>>,
            policy: self.policy,
            clock: Arc::new(move || db.load(Ordering::Relaxed)),
            claimed: None,
        });
        Node {
            name: name.to_owned(),
            logs,
            handles: BTreeMap::new(),
        }
    }

    fn rows(&self, thread: &str) -> Vec<(u64, String)> {
        block_on(self.reader.scan(thread, 0, SCAN_END)).expect("the model store answers every scan")
    }

    fn refold(&self, thread: &str, rows: &[(u64, String)]) -> Result<ThreadState, StoreError> {
        let (from, mut state) = match self.bases.borrow().get(thread) {
            Some((seq, base)) => (*seq, base.clone()),
            None => (0, self.fold.empty()),
        };
        for (seq, row) in rows.iter().filter(|(seq, _)| *seq >= from) {
            self.fold.apply(&mut state, *seq, row)?;
        }
        Ok(state)
    }
}

pub(crate) fn publish(world: &World, nodes: &[Node]) {
    world.net.nodes.set(
        nodes
            .iter()
            .map(|node| (node.name.clone(), node.logs.clone()))
            .collect(),
    );
}

fn drive<T>(time: &Arc<Clock>, future: impl Future<Output = T>) -> Option<T> {
    let mut future = std::pin::pin!(future);
    let mut context = Context::from_waker(Waker::noop());
    loop {
        if let Poll::Ready(value) = future.as_mut().poll(&mut context) {
            return Some(value);
        }
        if !time.advance_to_next() {
            return None;
        }
    }
}

pub(crate) fn driven<T>(world: &World, what: &str, future: impl Future<Output = T>) -> Option<T> {
    let driven = drive(&world.time, future);
    if driven.is_none() {
        flag(
            &world.wrong,
            format!("deadline: {what} waits with no timer left to fire"),
        );
    }
    driven
}

pub(crate) fn raw(value: &serde_json::Value) -> Box<RawValue> {
    serde_json::value::to_raw_value(value).expect("a JSON value encodes")
}

fn turn_of(rng: &mut Rng, turns: u64) -> String {
    format!("t{}", rng.below(turns.max(1)))
}

fn wake_command(rng: &mut Rng, state: &ThreadState) -> ThreadCommand {
    let armed: Vec<(String, u64)> = state
        .wakes()
        .map(|(key, wake)| (key.to_owned(), wake.generation))
        .collect();
    let records = if !armed.is_empty() && rng.chance(400_000) {
        let (key, generation) = armed[rng.pick(armed.len())].clone();
        vec![Record::Disarmed { generation, key }]
    } else {
        vec![Record::Armed {
            generation: state.generation() + 1,
            key: format!("w{}", rng.below(3)),
            wake: Wake {
                due_at: Some(rng.below(10_000)),
                on: None,
                payload: raw(&json!({ "wake": rng.below(100) })),
            },
        }]
    };
    ThreadCommand::Write { records }
}

fn command(rng: &mut Rng, turns: &mut u64, state: Option<&ThreadState>) -> ThreadCommand {
    let at = rng.below(1_000_000);
    let pick = rng.below(100);
    match pick {
        0..20 => {
            let request_id = format!("t{turns}");
            *turns += 1;
            ThreadCommand::Submit {
                submission: Submission {
                    request_id,
                    prompt: Prompt::new(format!("prompt {}", rng.below(1_000))),
                    hand: None,
                    metadata: raw(&json!({})),
                },
                at,
            }
        }
        20..34 => ThreadCommand::Start {
            request_id: turn_of(rng, *turns),
            at,
        },
        34..44 => ThreadCommand::Finish {
            request_id: turn_of(rng, *turns),
            state: if rng.chance(800_000) {
                TurnState::Completed
            } else {
                TurnState::Failed
            },
            fields: Map::new(),
            at,
        },
        44..49 => ThreadCommand::Cancel {
            request_id: turn_of(rng, *turns),
            fields: Map::new(),
            at,
        },
        49..63 => ThreadCommand::Append {
            entries: (0..1 + rng.below(3))
                .map(|at| LogEntry {
                    request_id: rng.chance(500_000).then(|| turn_of(rng, *turns)),
                    event: raw(&json!({ "type": "note", "n": at })),
                })
                .collect(),
            at,
        },
        63..75 => ThreadCommand::Write {
            records: vec![Record::Cell {
                cell: rng.below(CELLS),
                fresh: rng.chance(200_000),
                session: format!("s{}", rng.below(SESSIONS)),
                update: raw(&json!({ "update": rng.below(1_000) })),
            }],
        },
        75..78 => ThreadCommand::Write {
            records: vec![Record::Forget {
                cell: rng.below(CELLS),
                session: format!("s{}", rng.below(SESSIONS)),
            }],
        },
        78..83 => ThreadCommand::Facts {
            facts: vec![(
                Fact::Decision(Decision::Host {
                    key: format!("h{}", rng.below(3)),
                    value: raw(&json!(rng.below(1_000))),
                }),
                None,
            )],
        },
        83..87 => ThreadCommand::Write {
            records: vec![Record::Stored {
                key: format!("k{}", rng.below(4)),
                value: format!("v{}", rng.below(1_000)),
            }],
        },
        87..90 => ThreadCommand::Write {
            records: vec![Record::Checkpoint {
                payload: format!("payload {}", rng.below(1_000)),
                revision: rng.below(100),
                state: format!("state{}", rng.below(2)),
            }],
        },
        90..93 => ThreadCommand::Write {
            records: vec![Record::Hold {
                hold: Hold::Waiting {
                    call_id: format!("c{}", rng.below(1_000_000)),
                    card: raw(&json!({})),
                    input: raw(&json!({})),
                    tool: "approve".to_owned(),
                    turn: None,
                },
                key: format!("hold{}", rng.below(1_000_000)),
            }],
        },
        93..97 => match state {
            Some(state) => wake_command(rng, state),
            None => ThreadCommand::Pending,
        },
        _ => scoped(rng, *turns, state),
    }
}

fn scoped(rng: &mut Rng, turns: u64, state: Option<&ThreadState>) -> ThreadCommand {
    let owner = rng.chance(500_000).then(|| format!("state{}", rng.below(2)));
    let open = state.and_then(|state| {
        let intents: Vec<(String, Option<String>)> = state
            .intents()
            .map(|(call_id, held, _)| (call_id.to_owned(), held.map(str::to_owned)))
            .collect();
        (!intents.is_empty()).then(|| intents[rng.pick(intents.len())].clone())
    });
    let fact = match rng.below(4) {
        0 => Fact::Decision(Decision::Time {
            unix_millis: rng.below(1_000_000),
        }),
        1 => {
            let id = rng.below(1_000_000);
            return ThreadCommand::Facts {
                facts: vec![(
                    Fact::Intent(Intent {
                        turn: rng.chance(500_000).then(|| turn_of(rng, turns)),
                        call_id: format!("i{id}"),
                        tool: "deploy".to_owned(),
                        target: IntentTarget::Nested {
                            parent: format!("p{id}"),
                            input: raw(&json!({ "intent": id })),
                        },
                    }),
                    owner,
                )],
            };
        }
        2 => match open {
            Some((call_id, held)) => {
                return ThreadCommand::Facts {
                    facts: vec![(
                        Fact::EffectResult(EffectResult {
                            call_id,
                            output: raw(&json!({ "done": true })),
                        }),
                        held,
                    )],
                };
            }
            None => Fact::Decision(Decision::Time {
                unix_millis: rng.below(1_000_000),
            }),
        },
        _ => {
            return ThreadCommand::Write {
                records: vec![Record::Recovery {
                    at: None,
                    attempt: 1 + rng.below(3),
                    turn: turn_of(rng, turns),
                    unknown: Vec::new(),
                }],
            };
        }
    };
    ThreadCommand::Facts {
        facts: vec![(fact, Some(format!("state{}", rng.below(2))))],
    }
}

fn job_landed(world: &World, thread: &str, note: u64, event: u64, fact: u64) -> bool {
    let Ok(state) = world.refold(thread, &world.rows(thread)) else {
        return false;
    };
    let event = state
        .events()
        .find(|held| held.seq == event)
        .and_then(|held| serde_json::from_str::<serde_json::Value>(held.event.get()).ok());
    let fact = state.facts().find(|(seq, _)| *seq == fact).map(|(_, kept)| &kept.fact);
    let host = matches!(
        fact,
        Some(Fact::Decision(Decision::Host { key, value }))
            if key == "job" && value.get() == note.to_string()
    );
    event.is_some_and(|event| event["n"] == note) && host
}

#[derive(Debug, Default, Deserialize, PartialEq, Eq, Serialize)]
pub(crate) struct Tally {
    inputs: u64,
    bytes: u64,
    decisions: Vec<u64>,
}

impl DurableFold for Tally {
    const FAMILY: &'static str = "sim.tally";
    const FORMAT: u32 = 1;
    type Decision = u64;

    fn apply(&mut self, _seq: u64, fact: Seen<'_, u64>) -> Result<(), FoldError> {
        match fact {
            Fact::Input(input) => {
                self.inputs += 1;
                self.bytes += input.value.get().len() as u64;
            }
            Fact::Decision(Decision::Backend(value)) => self.decisions.push(*value),
            _ => {}
        }
        Ok(())
    }

    fn request(&self) -> Result<Vec<u8>, FoldError> {
        Ok(Vec::new())
    }
}

fn fact_input(rng: &mut Rng) -> OwnedFact<u64> {
    let bytes = if rng.chance(FACT_BLOB_PPM) {
        VALUE_INLINE_BYTES + usize::try_from(rng.below(64)).expect("a length fits")
    } else {
        1 + usize::try_from(rng.below(32)).expect("a length fits")
    };
    Fact::Input(Input {
        turn: "tally".to_owned(),
        kind: InputKind::Prompt,
        value: raw(&json!("x".repeat(bytes))),
        metadata: raw(&json!(null)),
        hand: None,
        first: None,
    })
}

fn fact_log(
    rng: &mut Rng,
    world: &World,
    log: ThreadLog,
    thread: &str,
    stats: &mut ThreadScopeStats,
) {
    let routed = log.state().is_none();
    let facts = ThreadFacts::new(Arc::new(log));
    let scoped = facts.scoped(&format!("state{}", rng.below(FACT_STATES)), rng.chance(500_000));
    stats.fact_logs += 1;
    let first = driven(world, "a fact replay", fold_facts::<Tally>(&*scoped));
    let second = driven(world, "a fact replay", fold_facts::<Tally>(&*scoped));
    let (Some(Ok(first)), Some(Ok(second))) = (first, second) else {
        if routed {
            stats.fact_logs_routed += 1;
        } else {
            stats.refused += 1;
        }
        return;
    };
    if routed {
        flag(
            &world.wrong,
            format!("facts: a fact log of {thread} replayed through a node that routes the thread"),
        );
        return;
    }
    let ahead: OwnedFact<u64> = Fact::Decision(Decision::Backend(rng.below(1_000)));
    let behind = fact_input(rng);
    let Some(Ok(_)) = driven(world, "a fact commit", commit_fact(&*scoped, first, ahead)) else {
        stats.refused += 1;
        return;
    };
    let committed = driven(
        world,
        "a stale fact commit",
        commit_fact(&*scoped, second, behind),
    );
    let Some(Ok((caught, seq))) = committed else {
        stats.refused += 1;
        return;
    };
    stats.fact_commits += 2;
    let Some(Ok(fresh)) = driven(world, "a fact replay", fold_facts::<Tally>(&*scoped)) else {
        return;
    };
    if fresh.seq() != caught.seq() || fresh.state() != caught.state() {
        flag(
            &world.wrong,
            format!(
                "facts: an append on {thread} behind the log's last fact committed fact {seq} and folds {:?} at {}, but a replay folds {:?} at {}",
                caught.state(),
                caught.seq(),
                fresh.state(),
                fresh.seq()
            ),
        );
    }
}

fn job(rng: &mut Rng, world: &World, log: &ThreadLog, thread: &str, stats: &mut ThreadScopeStats) {
    let note = rng.below(1_000_000);
    let at = rng.below(1_000_000);
    let routed = log.state().is_none();
    let ran = log.job(move |staged| {
        let event = staged.event(at, None, raw(&json!({ "type": "job", "n": note })))?;
        let host = Fact::Decision(Decision::Host {
            key: "job".to_owned(),
            value: raw(&json!(note)),
        });
        let fact = staged.fact(&host, None)?;
        Ok((event, fact))
    });
    stats.sends += 1;
    match driven(world, "a job", ran) {
        Some(Ok(_)) if routed => flag(
            &world.wrong,
            format!("a job on {thread} ran through a node that routes the thread to its holder"),
        ),
        Some(Ok((event, fact))) => {
            stats.committed += 1;
            stats.jobs += 1;
            if !job_landed(world, thread, note, event, fact) {
                flag(
                    &world.wrong,
                    format!(
                        "a job on {thread} answered event {event} and fact {fact}, which its committed rows do not hold"
                    ),
                );
            }
        }
        Some(Err(ScopeError::Routed { .. })) => stats.jobs_routed += 1,
        Some(Err(_)) | None => stats.refused += 1,
    }
}

fn send(
    rng: &mut Rng,
    world: &World,
    nodes: &mut [Node],
    turns: &mut BTreeMap<String, u64>,
    stats: &mut ThreadScopeStats,
) {
    let at = rng.pick(nodes.len());
    let thread = world.threads[rng.pick(world.threads.len())].clone();
    let Some(reached) = driven(world, "a get", nodes[at].logs.get(&thread)) else {
        return;
    };
    let Ok(log) = reached else {
        stats.refused += 1;
        return;
    };
    if log.state().is_some() {
        adopted(world, &thread, stats);
    }
    if rng.chance(FACT_LOG_PPM) {
        fact_log(rng, world, log, &thread, stats);
        return;
    }
    if rng.chance(JOB_PPM) {
        job(rng, world, &log, &thread, stats);
        if log.state().is_some() {
            nodes[at].handles.insert(thread, log);
        }
        return;
    }
    let state = log.state();
    let counter = turns.entry(thread.clone()).or_insert(0);
    let command = command(rng, counter, state.as_deref());
    stats.sends += 1;
    match driven(world, "a send", log.send(command)) {
        Some(Ok(_)) => stats.committed += 1,
        Some(Err(_)) | None => stats.refused += 1,
    }
    if log.state().is_some() {
        nodes[at].handles.insert(thread, log);
    }
}

fn resident(world: &World, nodes: &[Node], stats: &mut ThreadScopeStats) {
    for node in nodes {
        for (thread, log) in &node.handles {
            let (Some(state), Some(next)) = (log.state(), log.next()) else {
                continue;
            };
            let rows: Vec<(u64, String)> = world
                .rows(thread)
                .into_iter()
                .filter(|(seq, _)| *seq < next)
                .collect();
            stats.resident_checks += 1;
            match world.refold(thread, &rows) {
                Ok(refold) if refold.digest() == state.digest() => {}
                Ok(_) => flag(
                    &world.wrong,
                    format!(
                        "{} holds {thread} at row {next} with a state that differs from a refold of its committed rows",
                        node.name
                    ),
                ),
                Err(error) => flag(
                    &world.wrong,
                    format!("committed rows of {thread} do not refold: {error}"),
                ),
            }
        }
    }
}

fn decoded(world: &World, manifest: &Manifest) -> Result<ThreadState, String> {
    let digests: Vec<String> = manifest.parts.iter().map(|(_, at)| at.clone()).collect();
    let found: BTreeMap<String, String> = match block_on(world.witness.parts(&digests)) {
        Ok(found) => found.into_iter().collect(),
        Err(error) => return Err(error.to_string()),
    };
    let mut parts = Vec::with_capacity(manifest.parts.len());
    for (name, at) in &manifest.parts {
        let Some(body) = found.get(at) else {
            return Err(format!("part {name} has no body"));
        };
        parts.push(Part {
            name: name.clone(),
            body: body.clone(),
        });
    }
    world.fold.decode(parts).map_err(|error| error.to_string())
}

fn adopted(world: &World, thread: &str, stats: &mut ThreadScopeStats) {
    let Some((legacy, theirs)) = world.legacy.get(thread) else {
        return;
    };
    if world.bases.borrow().contains_key(thread) {
        return;
    }
    let Some(first) = world.witness.manifests(thread).first().cloned() else {
        flag(
            &world.wrong,
            format!("{thread} was claimed over its turn log history with no checkpoint after it"),
        );
        return;
    };
    if first.seq != legacy.next {
        flag(
            &world.wrong,
            format!(
                "the first checkpoint of {thread} is at row {}, not at its turn log's head {}",
                first.seq, legacy.next
            ),
        );
        return;
    }
    let state = match decoded(world, &first) {
        Ok(state) => state,
        Err(error) => {
            flag(&world.wrong, format!("the adopted checkpoint of {thread} does not decode: {error}"));
            return;
        }
    };
    stats.adopted_checks += 1;
    if let Some(difference) = threadlegacy::differs(theirs, &threadlegacy::ours(&state, legacy)) {
        flag(
            &world.wrong,
            format!("the adopted state of {thread} differs from its turn log's own view: {difference}"),
        );
        return;
    }
    world
        .bases
        .borrow_mut()
        .insert(thread.to_owned(), (first.seq, state));
}

fn cold(world: &World, thread: &str) -> Option<Result<ThreadState, String>> {
    let manifest = world.witness.manifests(thread).last().cloned()?;
    let mut state = match decoded(world, &manifest) {
        Ok(state) => state,
        Err(error) => return Some(Err(error)),
    };
    for (seq, row) in world
        .rows(thread)
        .iter()
        .filter(|(seq, _)| *seq >= manifest.seq)
    {
        if let Err(error) = world.fold.apply(&mut state, *seq, row) {
            return Some(Err(error.to_string()));
        }
    }
    Some(Ok(state))
}

fn colds(world: &World, stats: &mut ThreadScopeStats) {
    for thread in &world.threads {
        if !world.witness.manifests(thread).is_empty() {
            adopted(world, thread, stats);
        }
        let Some(loaded) = cold(world, thread) else {
            continue;
        };
        stats.cold_checks += 1;
        let refold = world.refold(thread, &world.rows(thread));
        match (loaded, refold) {
            (Ok(loaded), Ok(refold)) if loaded.digest() == refold.digest() => {}
            (Ok(_), Ok(_)) => flag(
                &world.wrong,
                format!(
                    "a cold claim of {thread} from its checkpoint and tail differs from a refold of its rows"
                ),
            ),
            (Err(error), _) => flag(
                &world.wrong,
                format!("a cold claim of {thread} from its checkpoint fails: {error}"),
            ),
            (_, Err(error)) => flag(
                &world.wrong,
                format!("committed rows of {thread} do not refold: {error}"),
            ),
        }
    }
}

fn phase(phase: &Phase) -> &'static str {
    match phase {
        Phase::Queued { .. } => "queued",
        Phase::Running {
            cancelling: true, ..
        } => "cancelling",
        Phase::Running { .. } => "running",
        Phase::Finished { .. } => "finished",
    }
}

fn legacy(world: &World, stats: &mut ThreadScopeStats) {
    for thread in &world.threads {
        if world.legacy.contains_key(thread) {
            continue;
        }
        let rows = world.rows(thread);
        let mut fold = Fold::default();
        for (seq, row) in &rows {
            if let Err(error) = fold.load(seq + 1, row) {
                flag(
                    &world.wrong,
                    format!("the turn log fold refuses row {seq} of {thread}: {error}"),
                );
                return;
            }
        }
        let Ok(ours) = world.refold(thread, &rows) else {
            return;
        };
        stats.legacy_checks += 1;
        let theirs: Vec<(String, &str, String, u64)> = fold
            .turns()
            .iter()
            .map(|turn| (turn.id.clone(), phase(&turn.phase), turn.digest.clone(), turn.first))
            .collect();
        let mine: Vec<(String, &str, String, u64)> = ours
            .turns()
            .map(|turn| (turn.id.clone(), phase(&turn.phase), turn.digest.clone(), turn.first))
            .collect();
        let events = |seqs: Vec<u64>| seqs;
        let same = theirs == mine
            && events(fold.events().iter().map(|event| event.seq).collect())
                == events(ours.events().map(|event| event.seq).collect())
            && fold
                .cells()
                .map(|(session, cell, updates)| (session.to_owned(), cell, updates.len()))
                .collect::<Vec<_>>()
                == ours
                    .cells()
                    .map(|(session, cell, updates)| (session.to_owned(), cell, updates.len()))
                    .collect::<Vec<_>>()
            && fold.holds().count() == ours.holds().count()
            && fold.next_fact() == ours.next_fact()
            && fold.open_calls().count() == ours.open_calls().count()
            && recovered(&fold) == recovered_ours(&ours);
        if !same {
            flag(
                &world.wrong,
                format!(
                    "the resident fold of {thread} disagrees with the turn log fold over the same rows: theirs {theirs:?}, ours {mine:?}"
                ),
            );
        }
    }
}

type Recovered = (Vec<String>, Vec<(String, Option<String>)>, Vec<(String, u64)>, bool);

fn recovered(fold: &Fold) -> Recovered {
    (
        fold.unknown(),
        fold.intents()
            .map(|(call_id, state, _)| (call_id.to_owned(), state.map(str::to_owned)))
            .collect(),
        fold.turns()
            .iter()
            .map(|turn| (turn.id.clone(), fold.next_attempt(&turn.id)))
            .collect(),
        fold.recovering(),
    )
}

fn recovered_ours(state: &ThreadState) -> Recovered {
    let mut turns: Vec<(String, u64)> = state
        .turns()
        .map(|turn| (turn.id.clone(), state.next_attempt(&turn.id)))
        .collect();
    turns.sort_by_key(|(id, _)| state.turn(id).map_or(0, |turn| turn.first));
    (
        state.unknown(),
        state
            .intents()
            .map(|(call_id, held, _)| (call_id.to_owned(), held.map(str::to_owned)))
            .collect(),
        turns,
        state.recovering(),
    )
}

fn step(
    rng: &mut Rng,
    world: &World,
    nodes: &mut Vec<Node>,
    generation: &mut u64,
    turns: &mut BTreeMap<String, u64>,
    stats: &mut ThreadScopeStats,
) {
    stats.steps += 1;
    world
        .db
        .fetch_add(rng.below(TICK_MS_MAX), Ordering::Relaxed);
    if rng.chance(CRASH_PPM) && nodes.len() > 1 {
        let at = rng.pick(nodes.len());
        let gone = nodes.remove(at);
        let runtime = gone.logs.scopes().stats();
        let (adoptions, rows) = world.gone.get();
        world
            .gone
            .set((adoptions + runtime.adoptions, rows + runtime.adoption_rows));
        stats.crashes += 1;
        *generation += 1;
        nodes.push(world.node(&format!("n{at}g{generation}")));
        drop(gone);
        publish(world, nodes);
    } else if rng.chance(RELEASE_PPM) {
        let at = rng.pick(nodes.len());
        let part = nanocode_partitions::Partition(u32::try_from(rng.below(u64::from(PARTS))).expect("a partition fits"));
        if driven(world, "a release", nodes[at].logs.scopes().release(part)).is_some() {
            stats.releases += 1;
        }
    } else if rng.chance(TICK_PPM) {
        let at = rng.pick(nodes.len());
        driven(world, "a tick", nodes[at].logs.scopes().tick());
    } else {
        send(rng, world, nodes, turns, stats);
    }
    for node in nodes.iter() {
        driven(world, "a renewal", node.logs.scopes().renew());
    }
    resident(world, nodes, stats);
    colds(world, stats);
}

struct Outcome {
    digest: u64,
    wrong: Option<String>,
    stats: ThreadScopeStats,
    shape: String,
}

fn run_seed(seed: u64, mutation: ThreadScopeMutation) -> Outcome {
    let time = Clock::new();
    crate::host::entered(seed, &time, || seeded(seed, mutation, &time))
}

fn world(rng: &mut Rng, time: &Arc<Clock>) -> World {
    let db = Arc::new(AtomicU64::new(START_MS));
    let clock = Arc::clone(&db);
    let store =
        Arc::new(ModelParts::new().with_clock(Arc::new(move || clock.load(Ordering::Relaxed))));
    let count = NonZeroU32::new(PARTS).expect("partitions are counted from one");
    block_on(store.migrate(count)).expect("the model store migrates");
    let thread = ThreadPolicy {
        part_rows: usize::try_from(rng.range(PART_ROWS.start, PART_ROWS.end))
            .expect("a part bound fits"),
        finished_max: None,
        row_bytes_max: None,
    };
    let policy = Policy {
        checkpoint_rows: usize::try_from(rng.range(CHECKPOINT_ROWS.start, CHECKPOINT_ROWS.end))
            .expect("a row bound fits"),
        checkpoint_ms: CHECKPOINT_MS,
        resident_bytes_max: usize::try_from(rng.range(RESIDENT_BYTES.start, RESIDENT_BYTES.end))
            .expect("a byte bound fits"),
        forward_ms: FORWARD_MS,
    };
    World {
        time: Arc::clone(time),
        db,
        store: Arc::clone(&store),
        witness: Arc::new(MemoryCheckpoints::new(Arc::clone(&store))),
        net: Arc::new(Net::default()),
        reader: PartedLog::new(Arc::clone(&store), count),
        fold: ThreadFold::new(thread),
        thread,
        policy,
        count,
        threads: (0..rng.range(THREADS.start, THREADS.end))
            .map(|at| format!("thread-{at}"))
            .collect(),
        legacy: BTreeMap::new(),
        bases: RefCell::default(),
        gone: Cell::new((0, 0)),
        wrong: Arc::default(),
    }
}

fn history(rng: &mut Rng, world: &mut World, stats: &mut ThreadScopeStats) {
    let host: Arc<dyn LogHost> = Arc::new(PartedLog::new(Arc::clone(&world.store), world.count));
    for thread in world.threads.clone() {
        if !rng.chance(LEGACY_PPM) {
            continue;
        }
        let written = driven(
            world,
            "a legacy history",
            threadlegacy::write(rng, Arc::clone(&host), &thread),
        );
        let legacy = match written {
            Some(Ok(legacy)) => legacy,
            Some(Err(error)) => {
                flag(&world.wrong, format!("the legacy history of {thread}: {error}"));
                return;
            }
            None => return,
        };
        let next = world.rows(&thread).last().map_or(0, |(seq, _)| seq + 1);
        let legacy = legacy.at(next);
        let theirs = match driven(
            world,
            "a legacy view",
            threadlegacy::expected(Arc::clone(&host), &thread, &legacy),
        ) {
            Some(Ok(theirs)) => theirs,
            Some(Err(error)) => {
                flag(&world.wrong, format!("the legacy view of {thread}: {error}"));
                return;
            }
            None => return,
        };
        stats.legacy_threads += 1;
        stats.legacy_turns += legacy.turns() as u64;
        stats.legacy_retired += theirs.retired;
        stats.legacy_blobs += theirs.blobs;
        stats.legacy_paged_rows += world
            .rows(&thread)
            .iter()
            .filter(|(_, body)| !body.starts_with('{'))
            .count() as u64;
        world.legacy.insert(thread, (legacy, theirs));
    }
}

fn seeded(seed: u64, mutation: ThreadScopeMutation, time: &Arc<Clock>) -> Outcome {
    let mut rng = Rng::derive(seed, "thread_scope");
    let mut stats = ThreadScopeStats {
        seeds: 1,
        ..ThreadScopeStats::default()
    };
    let mut world = world(&mut rng, time);
    history(&mut rng, &mut world, &mut stats);
    let mut nodes: Vec<Node> = (0..rng.range(NODES.start, NODES.end))
        .map(|at| world.node(&format!("n{at}g0")))
        .collect();
    publish(&world, &nodes);
    let steps = rng.range(STEPS.start, STEPS.end);
    let shape = format!(
        "nodes={} threads={} checkpoint_rows={} part_rows={} resident_bytes_max={} steps={steps}",
        nodes.len(),
        world.threads.len(),
        world.policy.checkpoint_rows,
        world.thread.part_rows,
        world.policy.resident_bytes_max
    );
    thread_scope_sim::mutate(mutation.0);
    scope_sim::mutate(mutation.1);
    let mut generation = 0;
    let mut turns = BTreeMap::new();
    for _ in 0..steps {
        step(&mut rng, &world, &mut nodes, &mut generation, &mut turns, &mut stats);
        if world.wrong.read(Option::is_some) {
            break;
        }
    }
    legacy(&world, &mut stats);
    if world.wrong.read(Option::is_none) {
        let turns = self::world(&mut rng, time);
        crate::threadturns::run(&mut rng, &turns, &mut stats);
        turns.net.nodes.set(Vec::new());
        if let Some(wrong) = turns.wrong.read(Clone::clone) {
            flag(&world.wrong, wrong);
        }
    }
    if world.wrong.read(Option::is_none) {
        let holds = self::world(&mut rng, time);
        crate::threadholds::run(&mut rng, &holds, &mut stats);
        holds.net.nodes.set(Vec::new());
        if let Some(wrong) = holds.wrong.read(Clone::clone) {
            flag(&world.wrong, wrong);
        }
    }
    if world.wrong.read(Option::is_none) {
        let wakes = self::world(&mut rng, time);
        crate::threadwakes::run(&mut rng, &wakes, &mut stats);
        wakes.net.nodes.set(Vec::new());
        if let Some(wrong) = wakes.wrong.read(Clone::clone) {
            flag(&world.wrong, wrong);
        }
    }
    thread_scope_sim::mutate(Mutation::None);
    scope_sim::mutate(ScopeMutation::None);
    let (adoptions, adoption_rows) = world.gone.get();
    stats.adoptions += adoptions;
    stats.adoption_rows += adoption_rows;
    for node in &nodes {
        let runtime = node.logs.scopes().stats();
        stats.adoptions += runtime.adoptions;
        stats.adoption_rows += runtime.adoption_rows;
        stats.loads += runtime.loads;
        stats.load_rows_max = stats.load_rows_max.max(runtime.load_rows_max);
        stats.checkpoints += runtime.checkpoints;
        stats.checkpoint_parts += runtime.checkpoint_parts;
        stats.checkpoint_parts_kept += runtime.checkpoint_parts_kept;
        stats.evictions += runtime.evictions;
        stats.forwards += runtime.forwards;
    }
    if stats.adoptions > world.legacy.len() as u64 {
        flag(
            &world.wrong,
            format!(
                "{} turn log histories were adopted {} times; a claim after the first read the history again",
                world.legacy.len(),
                stats.adoptions
            ),
        );
    }
    world.net.nodes.set(Vec::new());
    let mut digest = fnv(FNV_OFFSET, &seed.to_le_bytes());
    for thread in &world.threads {
        for (seq, row) in world.rows(thread) {
            digest = fnv(fnv(digest, &seq.to_le_bytes()), row.as_bytes());
        }
        for manifest in world.witness.manifests(thread) {
            digest = fnv(
                fnv(digest, &manifest.seq.to_le_bytes()),
                &manifest.epoch.to_le_bytes(),
            );
        }
    }
    Outcome {
        digest,
        wrong: world.wrong.read(Clone::clone),
        stats,
        shape,
    }
}

pub fn judge(
    seed: u64,
    mutation: ThreadScopeMutation,
    twice: bool,
    verbose: bool,
    totals: &mut ThreadScopeStats,
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

pub fn replay(seed: u64, mutation: ThreadScopeMutation) -> String {
    let mut command = format!("nanocode-sim --scenario thread_scope --seed {seed}");
    if mutation != ThreadScopeMutation::NONE {
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
    mutation: ThreadScopeMutation,
    totals: &ThreadScopeStats,
) -> ExitCode {
    println!(
        "RESULT seeds={}..{} scenario=thread_scope mutation={} passed={passed} failed={failed} twice={twice} stats={totals:?}",
        seeds.start,
        seeds.end,
        mutation.name()
    );
    match mutation {
        ThreadScopeMutation::NONE if failed == 0 => ExitCode::SUCCESS,
        ThreadScopeMutation::NONE => ExitCode::FAILURE,
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
