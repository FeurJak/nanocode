use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    num::NonZeroU32,
    ops::Range,
    process::ExitCode,
};

use nanocode_partitions::{Partition, Token, assignment, plan};

use crate::{
    partitions::{NodeId, Record, Store, TICK_MS, TTL_MS, thread_partition},
    rng::{FNV_OFFSET, PPM, Rng, fnv},
};

const STEP_CPU_US: u64 = 119;
const CLAIM_CPU_US: u64 = 5;
const FOLD_US: u64 = 2;
const RECORDS_PER_THREAD: u64 = 454;
const NET_RTT_US: u64 = 409;
const STORE_TXN_US: u64 = 171;
const STORE_ROW_US: u64 = 26;
const STORE_FSYNC_US: u64 = 25;
const STORE_READ_US: u64 = 2;
const SYNC_FOLD_US: u64 = 1;
const SYNC_WATCH_US: u64 = 2;
const RECORDS_PER_STEP: u64 = 2;
const STEP_US: u64 =
    STEP_CPU_US + CLAIM_CPU_US + RECORDS_PER_STEP * (FOLD_US + SYNC_FOLD_US + SYNC_WATCH_US);
const CATCH_UP_US: u64 = RECORDS_PER_THREAD * (STORE_READ_US + SYNC_FOLD_US);
const WRITE_US: u64 = STORE_TXN_US + STORE_ROW_US + STORE_FSYNC_US;
const US_PER_MS: u64 = 1_000;
const MS_PER_S: u64 = 1_000;
const TICK_US: u64 = TICK_MS * US_PER_MS;
const MEASURE_TICKS: u64 = TTL_MS / TICK_MS;
const SETTLE_TICKS: u64 = 2 * TTL_MS / TICK_MS;
const NODES_MAX: usize = 10;
const SPEEDUP_MIN: u64 = 8;
const AGENT_COUNTS: [u32; 2] = [1_000, 10_000];
const PARTITIONS_MIN: u64 = 64;
const PARTITIONS_MAX: u64 = 256;
const MILLI: u64 = 1_000;
const PERCENT: u64 = 100;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScaleMutation {
    None,
    GlobalLock,
    RowPerRecord,
    OneNode,
    DoubleClaim,
}

impl ScaleMutation {
    const ALL: [Self; 5] = [
        Self::None,
        Self::GlobalLock,
        Self::RowPerRecord,
        Self::OneNode,
        Self::DoubleClaim,
    ];

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|mutation| mutation.name() == name)
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::GlobalLock => "global_lock",
            Self::RowPerRecord => "row_per_record",
            Self::OneNode => "one_node",
            Self::DoubleClaim => "double_claim",
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

    const fn commit_us(self, records: u64) -> u64 {
        match self {
            Self::GlobalLock => records * WRITE_US,
            Self::RowPerRecord => STORE_TXN_US + STORE_FSYNC_US + records * STORE_ROW_US,
            Self::None | Self::OneNode | Self::DoubleClaim => WRITE_US,
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct Stage {
    steps_per_s: u64,
    claims_per_s: u64,
    store_ppm: u64,
    warmup_ms: u64,
}

#[derive(Clone, Debug, Default)]
pub struct ScaleStats {
    pub seeds: u64,
    runs: [u64; AGENT_COUNTS.len()],
    steps: [[u64; NODES_MAX]; AGENT_COUNTS.len()],
    claims: [[u64; NODES_MAX]; AGENT_COUNTS.len()],
    store_ppm: [[u64; NODES_MAX]; AGENT_COUNTS.len()],
    speedup_min: [Option<u64>; AGENT_COUNTS.len()],
    speedup_max: [u64; AGENT_COUNTS.len()],
}

impl ScaleStats {
    pub fn add(&mut self, other: &Self) {
        self.seeds += other.seeds;
        for size in 0..AGENT_COUNTS.len() {
            self.runs[size] += other.runs[size];
            for stage in 0..NODES_MAX {
                self.steps[size][stage] += other.steps[size][stage];
                self.claims[size][stage] += other.claims[size][stage];
                self.store_ppm[size][stage] += other.store_ppm[size][stage];
            }
            self.speedup_min[size] = match (self.speedup_min[size], other.speedup_min[size]) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            };
            self.speedup_max[size] = self.speedup_max[size].max(other.speedup_max[size]);
        }
    }

    fn record(&mut self, size: usize, stages: &[Stage]) {
        if stages.len() < NODES_MAX {
            return;
        }
        self.runs[size] += 1;
        for (at, stage) in stages.iter().enumerate() {
            self.steps[size][at] += stage.steps_per_s;
            self.claims[size][at] += stage.claims_per_s;
            self.store_ppm[size][at] += stage.store_ppm;
        }
        let speedup = stages[NODES_MAX - 1].steps_per_s * MILLI / stages[0].steps_per_s.max(1);
        self.speedup_min[size] =
            Some(self.speedup_min[size].map_or(speedup, |min| min.min(speedup)));
        self.speedup_max[size] = self.speedup_max[size].max(speedup);
    }

    fn summary(&self) -> String {
        let mut out = format!("seeds={}", self.seeds);
        for (size, agents) in AGENT_COUNTS.iter().enumerate() {
            let runs = self.runs[size].max(1);
            let list = |values: &[u64; NODES_MAX], scale: u64| {
                values
                    .iter()
                    .map(|value| (value / runs / scale).to_string())
                    .collect::<Vec<_>>()
                    .join(",")
            };
            out.push_str(&format!(
                " agents{agents}=[runs={} steps_per_s={} claims_per_s={} store_util_pct={} speedup_min_x1000={} speedup_max_x1000={}]",
                self.runs[size],
                list(&self.steps[size], 1),
                list(&self.claims[size], 1),
                list(&self.store_ppm[size], PPM / PERCENT),
                self.speedup_min[size].unwrap_or(0),
                self.speedup_max[size]
            ));
        }
        out
    }
}

#[derive(Clone, Copy)]
struct Agent {
    partition: Partition,
    logged: u32,
    checked: u32,
    ready_at: u64,
}

struct Node {
    id: NodeId,
    clock: u64,
    held: BTreeMap<Partition, u64>,
    queue: VecDeque<u32>,
}

struct World {
    mutation: ScaleMutation,
    count: NonZeroU32,
    store: Store,
    nodes: Vec<Node>,
    node_base: u32,
    agents: Vec<Agent>,
    by_partition: BTreeMap<Partition, Vec<u32>>,
    pending: BTreeMap<Partition, u64>,
    cursor: u32,
    now: u64,
    store_clock: u64,
    store_busy: u64,
    committed: u64,
    claims: u64,
}

impl World {
    fn new(seed: u64, agents: u32, mutation: ScaleMutation) -> (Self, String) {
        let mut rng = Rng::derive(seed, &format!("scale-{agents}"));
        let count = NonZeroU32::new(
            u32::try_from(rng.range(PARTITIONS_MIN, PARTITIONS_MAX))
                .expect("a partition count fits a u32"),
        )
        .expect("a partition count is at least PARTITIONS_MIN");
        let thread_base = u32::try_from(rng.below(u64::from(u32::MAX - agents)))
            .expect("a thread base below u32::MAX fits a u32");
        let node_base = u32::try_from(rng.below(u64::from(u32::MAX) - NODES_MAX as u64))
            .expect("a node base below u32::MAX fits a u32");
        let mut by_partition: BTreeMap<Partition, Vec<u32>> = BTreeMap::new();
        let agents = (0..agents)
            .map(|index| {
                let partition = thread_partition(thread_base + index, count);
                by_partition.entry(partition).or_default().push(index);
                Agent {
                    partition,
                    logged: 0,
                    checked: 0,
                    ready_at: 0,
                }
            })
            .collect::<Vec<_>>();
        let shape = format!(
            "agents={} partitions={count} thread_base={thread_base} node_base={node_base}",
            agents.len()
        );
        (
            Self {
                mutation,
                count,
                store: Store::new(count),
                nodes: Vec::new(),
                node_base,
                agents,
                by_partition,
                pending: BTreeMap::new(),
                cursor: 0,
                now: 0,
                store_clock: 0,
                store_busy: 0,
                committed: 0,
                claims: 0,
            },
            shape,
        )
    }

    fn store_write(&mut self, cost: u64) {
        self.store_clock = self.store_clock.max(self.now) + cost;
        self.store_busy += cost;
    }

    fn join(&mut self) {
        let index = u32::try_from(self.nodes.len()).expect("a node index fits a u32");
        self.nodes.push(Node {
            id: NodeId::new(self.node_base + index),
            clock: self.now,
            held: BTreeMap::new(),
            queue: VecDeque::new(),
        });
    }

    fn live(&self) -> BTreeSet<NodeId> {
        let live = self.store.alive(self.now / US_PER_MS, TTL_MS);
        match (self.mutation, live.first()) {
            (ScaleMutation::OneNode, Some(first)) => BTreeSet::from([*first]),
            _ => live,
        }
    }

    fn lease(&mut self, at: usize) -> Result<(), String> {
        let now_ms = self.now / US_PER_MS;
        let id = self.nodes[at].id;
        self.store.heartbeat(id, now_ms);
        self.store_write(WRITE_US);
        let live = self.live();
        let mut steps = plan(
            &id,
            self.count,
            &live,
            &self.nodes[at].held,
            &self.store.rows,
            now_ms,
        );
        if self.mutation == ScaleMutation::DoubleClaim {
            steps.claim = assignment(self.count, &live)
                .into_iter()
                .filter(|(partition, owner)| {
                    *owner == id && !self.nodes[at].held.contains_key(partition)
                })
                .map(|(partition, _)| partition)
                .collect();
        }
        for token in steps.release {
            self.store
                .release(token.partition, id, token.epoch)
                .map_err(|fenced| {
                    format!("invariant 1 (single holder): node {at} release fenced: {fenced:?}")
                })?;
            self.store_write(WRITE_US);
            let node = &mut self.nodes[at];
            node.held.remove(&token.partition);
            let agents = &self.agents;
            node.queue
                .retain(|agent| agents[*agent as usize].partition != token.partition);
        }
        for partition in steps.claim {
            let holder_live = self.mutation != ScaleMutation::DoubleClaim
                && self.store.rows[&partition]
                    .holder()
                    .is_some_and(|holder| live.contains(holder));
            let Ok((_, epoch)) = self.store.claim(partition, id, now_ms, TTL_MS, holder_live)
            else {
                continue;
            };
            self.claims += 1;
            self.store_write(WRITE_US);
            let moved = self
                .by_partition
                .get(&partition)
                .cloned()
                .unwrap_or_default();
            let node = &mut self.nodes[at];
            node.held.insert(partition, epoch);
            node.clock += CLAIM_CPU_US + moved.len() as u64 * CATCH_UP_US;
            node.queue.extend(moved);
        }
        let held = self.nodes[at].held.clone();
        for (partition, epoch) in &held {
            self.store
                .renew(*partition, id, *epoch, now_ms, TTL_MS)
                .map_err(|fenced| {
                    format!("invariant 1 (single holder): node {at} renew fenced: {fenced:?}")
                })?;
        }
        self.claims += held.len() as u64;
        self.store_write(STORE_TXN_US + STORE_FSYNC_US + held.len() as u64 * STORE_ROW_US);
        Ok(())
    }

    fn run_steps(&mut self, at: usize, end: u64) -> Result<(), String> {
        let node = &mut self.nodes[at];
        while node.clock < end {
            let Some(agent) = node.queue.pop_front() else {
                break;
            };
            let state = &mut self.agents[agent as usize];
            if state.ready_at >= end {
                node.queue.push_front(agent);
                break;
            }
            node.clock = node.clock.max(state.ready_at);
            let partition = state.partition;
            let token = Token {
                partition,
                epoch: node.held[&partition],
            };
            let step = state.logged / 2;
            for record in [
                Record::Started {
                    thread: agent,
                    step,
                    hand: None,
                },
                Record::Done {
                    thread: agent,
                    step,
                },
            ] {
                self.store
                    .append(token, node.id, record)
                    .map_err(|fenced| {
                        format!("invariant 1 (single holder): node {at} append fenced: {fenced:?}")
                    })?;
            }
            state.logged += 2;
            node.clock += STEP_US;
            state.ready_at = node.clock + NET_RTT_US;
            *self.pending.entry(partition).or_default() += RECORDS_PER_STEP;
            node.queue.push_back(agent);
        }
        Ok(())
    }

    fn commit(&mut self, end: u64) {
        self.store_clock = self.store_clock.max(self.now);
        let order = (self.cursor..self.count.get()).chain(0..self.cursor);
        for partition in order.map(Partition) {
            if self.store_clock >= end {
                self.cursor = partition.0;
                return;
            }
            let Some(records) = self.pending.remove(&partition) else {
                continue;
            };
            let cost = self.mutation.commit_us(records);
            self.store_clock += cost;
            self.store_busy += cost;
            self.committed += records;
        }
        self.cursor = 0;
    }

    fn check_logs(&mut self) -> Result<(), String> {
        for log in self.store.logs.values_mut() {
            for entry in log.drain(..) {
                let (thread, step) = entry.record.key();
                let agent = self.agents.get_mut(thread as usize).ok_or_else(|| {
                    format!("invariant 2 (one run per step): unknown thread {thread}")
                })?;
                let fits = step == agent.checked / 2
                    && match entry.record {
                        Record::Started { .. } => agent.checked % 2 == 0,
                        Record::Done { .. } => agent.checked % 2 == 1,
                        Record::Uncertain { .. } => false,
                    };
                if !fits {
                    return Err(format!(
                        "invariant 2 (one run per step): thread {thread} logged {:?} after {} records",
                        entry.record, agent.checked
                    ));
                }
                agent.checked += 1;
            }
        }
        match self
            .agents
            .iter()
            .position(|agent| agent.checked != agent.logged)
        {
            Some(thread) => Err(format!(
                "invariant 2 (one run per step): thread {thread} ran {} records but the store logged {}",
                self.agents[thread].logged, self.agents[thread].checked
            )),
            None => Ok(()),
        }
    }

    fn tick(&mut self) -> Result<(), String> {
        let end = self.now + TICK_US;
        for at in 0..self.nodes.len() {
            self.nodes[at].clock = self.nodes[at].clock.max(self.now);
            self.lease(at)?;
            self.run_steps(at, end)?;
        }
        self.commit(end);
        self.check_logs()?;
        self.now = end;
        Ok(())
    }

    fn settled(&self) -> bool {
        let live = self.live();
        let now_ms = self.now / US_PER_MS;
        self.nodes
            .iter()
            .all(|node| node.clock < self.now + STEP_US)
            && assignment(self.count, &live)
                .iter()
                .all(|(partition, owner)| {
                    self.store.rows[partition].holder() == Some(owner)
                        && self
                            .nodes
                            .iter()
                            .any(|node| node.id == *owner && node.held.contains_key(partition))
                })
            && self.nodes.iter().all(|node| {
                plan(
                    &node.id,
                    self.count,
                    &live,
                    &node.held,
                    &self.store.rows,
                    now_ms,
                )
                .release
                .is_empty()
            })
    }

    fn stage(&mut self) -> Result<Stage, String> {
        self.join();
        let catch_up_ticks = (self.agents.len() as u64 * CATCH_UP_US).div_ceil(TICK_US);
        let warmup_max = catch_up_ticks + SETTLE_TICKS;
        let started = self.now;
        let mut ticks = 0;
        loop {
            self.tick()?;
            ticks += 1;
            if self.settled() {
                break;
            }
            if ticks > warmup_max {
                return Err(format!(
                    "invariant 3 (progress): {} nodes not settled after {warmup_max} ticks",
                    self.nodes.len()
                ));
            }
        }
        let warmup_ms = (self.now - started) / US_PER_MS;
        let (committed, claims, busy) = (self.committed, self.claims, self.store_busy);
        for _ in 0..MEASURE_TICKS {
            self.tick()?;
        }
        let window_us = MEASURE_TICKS * TICK_US;
        let per_s = |count: u64| count * MS_PER_S * US_PER_MS / window_us;
        Ok(Stage {
            steps_per_s: per_s((self.committed - committed) / RECORDS_PER_STEP),
            claims_per_s: per_s(self.claims - claims),
            store_ppm: (self.store_busy - busy) * PPM / window_us,
            warmup_ms,
        })
    }
}

pub struct ScaleOutcome {
    pub digest: u64,
    pub wrong: Option<String>,
    pub stats: ScaleStats,
    pub shape: String,
    lines: Vec<String>,
}

fn simulate(seed: u64, mutation: ScaleMutation) -> ScaleOutcome {
    let mut stats = ScaleStats {
        seeds: 1,
        ..ScaleStats::default()
    };
    let mut digest = FNV_OFFSET;
    let mut shapes = Vec::new();
    let mut lines = Vec::new();
    let mut wrong = None;
    for (size, agents) in AGENT_COUNTS.into_iter().enumerate() {
        let (mut world, shape) = World::new(seed, agents, mutation);
        shapes.push(shape.clone());
        let mut stages = Vec::new();
        for _ in 0..NODES_MAX {
            match world.stage() {
                Ok(stage) => stages.push(stage),
                Err(message) => {
                    wrong = Some(format!(
                        "{shape} at {} ms: {message}",
                        world.now / US_PER_MS
                    ));
                    break;
                }
            }
        }
        let base = stages.first().map_or(1, |stage| stage.steps_per_s.max(1));
        for (at, stage) in stages.iter().enumerate() {
            for value in [
                stage.steps_per_s,
                stage.claims_per_s,
                stage.store_ppm,
                stage.warmup_ms,
            ] {
                digest = fnv(digest, &value.to_le_bytes());
            }
            lines.push(format!(
                "  agents={agents} nodes={} steps_per_s={} claims_per_s={} store_util_pct={} speedup={}.{:03} warmup_ms={}",
                at + 1,
                stage.steps_per_s,
                stage.claims_per_s,
                stage.store_ppm / (PPM / PERCENT),
                stage.steps_per_s / base,
                stage.steps_per_s * MILLI / base % MILLI,
                stage.warmup_ms
            ));
        }
        stats.record(size, &stages);
        if wrong.is_some() {
            break;
        }
        let last = stages[NODES_MAX - 1].steps_per_s;
        if last < SPEEDUP_MIN * stages[0].steps_per_s {
            wrong = Some(format!(
                "{shape}: invariant 4 (scale-out): {NODES_MAX} nodes ran {last} steps/s, under {SPEEDUP_MIN}x the {} steps/s of 1 node",
                stages[0].steps_per_s
            ));
            break;
        }
    }
    ScaleOutcome {
        digest,
        wrong,
        stats,
        shape: shapes.join(" "),
        lines,
    }
}

pub fn judge(
    seed: u64,
    mutation: ScaleMutation,
    twice: bool,
    verbose: bool,
    totals: &mut ScaleStats,
) -> Option<String> {
    let first = simulate(seed, mutation);
    totals.add(&first.stats);
    if verbose {
        println!("seed {seed} digest {:016x} {}", first.digest, first.shape);
        for line in &first.lines {
            println!("{line}");
        }
    }
    if first.wrong.is_some() || !twice {
        return first.wrong;
    }
    let second = simulate(seed, mutation);
    (second.digest != first.digest || second.wrong.is_some()).then(|| {
        format!(
            "{} :: nondeterminism: digest {:016x} then {:016x}",
            first.shape, first.digest, second.digest
        )
    })
}

pub fn replay(seed: u64, mutation: ScaleMutation) -> String {
    let mut command = format!("nanocode-sim --scenario scale --seed {seed}");
    if mutation != ScaleMutation::None {
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
    mutation: ScaleMutation,
    totals: &ScaleStats,
) -> ExitCode {
    println!(
        "RESULT seeds={}..{} scenario=scale mutation={} passed={passed} failed={failed} twice={twice} stats={}",
        seeds.start,
        seeds.end,
        mutation.name(),
        totals.summary()
    );
    match mutation {
        ScaleMutation::None if failed == 0 => ExitCode::SUCCESS,
        ScaleMutation::None => ExitCode::FAILURE,
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
