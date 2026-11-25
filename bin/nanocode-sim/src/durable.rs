use std::{
    collections::{BTreeMap, BTreeSet},
    ops::Range,
    process::ExitCode,
};

use nanocode_durability_core::Lease;

use crate::rng::{FNV_OFFSET, Rng, fnv};

const STATES: usize = 3;
const OWNERS_MIN: u64 = 3;
const OWNERS_MAX: u64 = 5;
const STEPS_MIN: u64 = 400;
const STEPS_MAX: u64 = 1_600;
const STATEMENT_MS: u64 = 300;
const TTL_MS: u64 = 1_000;
const LEASE: Lease = Lease {
    ttl_ms: TTL_MS,
    grace_ms: STATEMENT_MS,
};
const PURGE_GRACE_MS: u64 = STATEMENT_MS;
const TICK_MS_MAX: u64 = 12;
const START_MS: u64 = 1_000_000;
const OWN_KEYS: u64 = 4;
const SHARED_KEYS: u64 = 2;
const REFS_MAX: u64 = 4;
const COMMIT_PPM: u64 = 500_000;
const COLLECT_PPM: u64 = 300_000;
const RELEASE_PPM: u64 = 50_000;
const CRASH_PPM: u64 = 40_000;
const PURGE_PPM: u64 = 600_000;
const SHARED_PPM: u64 = 600_000;
const OWNED_PPM: u64 = 900_000;
const STALL_MS_MAX: u64 = 3 * TTL_MS;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DurableMutation {
    None,
    WriteSkipsFence,
    ClaimStealsLive,
    ClaimSkipsGrace,
    PurgeSkipsGrace,
    CommitSkipsRevive,
    CollectFencesOwner,
}

impl DurableMutation {
    const ALL: [Self; 7] = [
        Self::None,
        Self::WriteSkipsFence,
        Self::ClaimStealsLive,
        Self::ClaimSkipsGrace,
        Self::PurgeSkipsGrace,
        Self::CommitSkipsRevive,
        Self::CollectFencesOwner,
    ];

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|mutation| mutation.name() == name)
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::WriteSkipsFence => "write_skips_fence",
            Self::ClaimStealsLive => "claim_steals_live",
            Self::ClaimSkipsGrace => "claim_skips_grace",
            Self::PurgeSkipsGrace => "purge_skips_grace",
            Self::CommitSkipsRevive => "commit_skips_revive",
            Self::CollectFencesOwner => "collect_fences_owner",
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
pub struct DurableStats {
    pub seeds: u64,
    pub steps: u64,
    pub claimed: u64,
    pub claims_busy: u64,
    pub takeovers_expired: u64,
    pub takeovers_released: u64,
    pub commits: u64,
    pub commits_fenced: u64,
    pub collects: u64,
    pub collects_refused: u64,
    pub stamped: u64,
    pub revived: u64,
    pub purged: u64,
    pub lock_waits: u64,
    pub timeouts: u64,
    pub stalls: u64,
    pub crashes: u64,
    pub releases: u64,
}

impl DurableStats {
    const fn counters(&self) -> [(&'static str, u64); 17] {
        [
            ("steps", self.steps),
            ("claimed", self.claimed),
            ("claims_busy", self.claims_busy),
            ("takeovers_expired", self.takeovers_expired),
            ("takeovers_released", self.takeovers_released),
            ("commits", self.commits),
            ("commits_fenced", self.commits_fenced),
            ("collects", self.collects),
            ("collects_refused", self.collects_refused),
            ("stamped", self.stamped),
            ("revived", self.revived),
            ("purged", self.purged),
            ("lock_waits", self.lock_waits),
            ("timeouts", self.timeouts),
            ("stalls", self.stalls),
            ("crashes", self.crashes),
            ("releases", self.releases),
        ]
    }

    pub fn add(&mut self, other: &Self) {
        let sum = |mine: u64, theirs: u64| mine + theirs;
        *self = Self {
            seeds: sum(self.seeds, other.seeds),
            steps: sum(self.steps, other.steps),
            claimed: sum(self.claimed, other.claimed),
            claims_busy: sum(self.claims_busy, other.claims_busy),
            takeovers_expired: sum(self.takeovers_expired, other.takeovers_expired),
            takeovers_released: sum(self.takeovers_released, other.takeovers_released),
            commits: sum(self.commits, other.commits),
            commits_fenced: sum(self.commits_fenced, other.commits_fenced),
            collects: sum(self.collects, other.collects),
            collects_refused: sum(self.collects_refused, other.collects_refused),
            stamped: sum(self.stamped, other.stamped),
            revived: sum(self.revived, other.revived),
            purged: sum(self.purged, other.purged),
            lock_waits: sum(self.lock_waits, other.lock_waits),
            timeouts: sum(self.timeouts, other.timeouts),
            stalls: sum(self.stalls, other.stalls),
            crashes: sum(self.crashes, other.crashes),
            releases: sum(self.releases, other.releases),
        };
    }

    fn summary(&self) -> String {
        let mut line = format!("seeds={}", self.seeds);
        for (name, value) in self.counters() {
            line.push_str(&format!(" {name}={value}"));
        }
        line
    }

    fn uncovered(&self) -> Vec<&'static str> {
        self.counters()
            .into_iter()
            .filter(|(_, value)| *value == 0)
            .map(|(name, _)| name)
            .collect()
    }
}

/// One owners row: the lease, fence and revision a write's compare-and-set checks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Head {
    owner: Option<usize>,
    fence: u64,
    revision: u64,
    expires: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Record {
    holder: Option<usize>,
    orphaned: Option<u64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Row {
    Owner(usize),
    Record(u64),
    Ref(u64, usize),
}

/// The committed tables: owners, states (each state's referenced keys), records and state_records.
#[derive(Clone, Default)]
struct Tables {
    heads: BTreeMap<usize, Head>,
    payloads: BTreeMap<usize, BTreeSet<u64>>,
    records: BTreeMap<u64, Record>,
    refs: BTreeSet<(u64, usize)>,
}

/// A transaction's uncommitted writes; None deletes.
#[derive(Default)]
struct Overlay {
    heads: BTreeMap<usize, Head>,
    payloads: BTreeMap<usize, BTreeSet<u64>>,
    records: BTreeMap<u64, Option<Record>>,
    refs: BTreeMap<(u64, usize), bool>,
}

#[derive(Clone, Debug)]
enum Kind {
    Claim {
        state: usize,
    },
    Commit {
        state: usize,
        fence: u64,
        expected: u64,
        refs: BTreeSet<u64>,
        sent: BTreeSet<u64>,
    },
    Collect {
        state: usize,
        fence: u64,
        revision: u64,
        dead: BTreeSet<u64>,
    },
    Release {
        state: usize,
        fence: u64,
    },
    Purge,
}

struct Txn {
    actor: usize,
    kind: Kind,
    statement: usize,
    began: u64,
    landed: bool,
    overlay: Overlay,
    locks: BTreeSet<Row>,
}

#[derive(Clone, Default)]
struct Belief {
    fence: u64,
    revision: u64,
    refs: BTreeSet<u64>,
    held: BTreeSet<u64>,
}

#[derive(Default)]
struct Actor {
    beliefs: BTreeMap<usize, Belief>,
    stalled_until: u64,
}

enum Ran {
    Next,
    Wait,
    Done,
}

struct World {
    rng: Rng,
    mutation: DurableMutation,
    now: u64,
    tables: Tables,
    claimed: BTreeMap<usize, (usize, u64)>,
    actors: Vec<Actor>,
    open: BTreeMap<usize, Txn>,
    locks: BTreeMap<Row, usize>,
    stats: DurableStats,
    wrong: Option<String>,
}

fn key_name(key: u64) -> String {
    if key >= 1_000 {
        format!("s{}/k{}", key / 1_000 - 1, key % 1_000)
    } else {
        format!("shared/x{key}")
    }
}

impl World {
    fn head(&self, txn: &Txn, state: usize) -> Option<Head> {
        txn.overlay
            .heads
            .get(&state)
            .copied()
            .or_else(|| self.tables.heads.get(&state).copied())
    }

    fn record(&self, txn: &Txn, key: u64) -> Option<Record> {
        match txn.overlay.records.get(&key) {
            Some(record) => *record,
            None => self.tables.records.get(&key).copied(),
        }
    }

    fn referenced(&self, txn: &Txn, key: u64) -> bool {
        let committed = self
            .tables
            .refs
            .range((key, 0)..(key + 1, 0))
            .any(|row| txn.overlay.refs.get(row).copied().unwrap_or(true));
        committed
            || txn
                .overlay
                .refs
                .iter()
                .any(|((ref_key, _), present)| *ref_key == key && *present)
    }

    fn has_ref(&self, txn: &Txn, key: u64, state: usize) -> bool {
        txn.overlay
            .refs
            .get(&(key, state))
            .copied()
            .unwrap_or_else(|| self.tables.refs.contains(&(key, state)))
    }

    fn blocked(&self, actor: usize, rows: &[Row]) -> bool {
        rows.iter()
            .any(|row| self.locks.get(row).is_some_and(|holder| *holder != actor))
    }

    fn lock(&mut self, txn: &mut Txn, rows: &[Row]) {
        for row in rows {
            self.locks.insert(*row, txn.actor);
            txn.locks.insert(*row);
        }
    }

    fn gated(&self, txn: &Txn, state: usize, fence: u64, revision: u64) -> bool {
        txn.landed
            && self.head(txn, state).is_some_and(|head| {
                head.owner == Some(txn.actor) && head.fence == fence && head.revision == revision
            })
    }

    /// CLAIM: lands on a released row, the claimer's own, or one expired past grace.
    fn claim(&mut self, txn: &mut Txn, state: usize) -> Ran {
        if self.blocked(txn.actor, &[Row::Owner(state)]) {
            return Ran::Wait;
        }
        let now = self.now;
        let next = match self.head(txn, state) {
            None => Some(Head {
                owner: Some(txn.actor),
                fence: 1,
                revision: 0,
                expires: now + LEASE.ttl_ms,
            }),
            Some(head) => {
                let claimable = match self.mutation {
                    DurableMutation::ClaimStealsLive => true,
                    DurableMutation::ClaimSkipsGrace => {
                        head.owner.is_none() || head.owner == Some(txn.actor) || head.expires < now
                    }
                    _ => {
                        head.owner.is_none()
                            || head.owner == Some(txn.actor)
                            || head.expires + LEASE.grace_ms < now
                    }
                };
                claimable.then_some(Head {
                    owner: Some(txn.actor),
                    fence: head.fence + 1,
                    revision: head.revision,
                    expires: now + LEASE.ttl_ms,
                })
            }
        };
        if let Some(next) = next {
            txn.overlay.heads.insert(state, next);
            txn.landed = true;
            self.lock(txn, &[Row::Owner(state)]);
        }
        Ran::Done
    }

    /// HEAD_ONE / GROUP_HEADS_PG: the owners-row compare-and-set, then the state row.
    fn commit_head(&mut self, txn: &mut Txn) -> Ran {
        let Kind::Commit {
            state,
            fence,
            expected,
            ref refs,
            ..
        } = txn.kind
        else {
            unreachable!("commit_head runs only for a commit");
        };
        if self.blocked(txn.actor, &[Row::Owner(state)]) {
            return Ran::Wait;
        }
        let refs = refs.clone();
        if let Some(head) = self.head(txn, state) {
            let fenced = head.owner == Some(txn.actor) && head.fence == fence;
            let matches = head.revision == expected
                && (fenced || self.mutation == DurableMutation::WriteSkipsFence);
            if matches {
                txn.overlay.heads.insert(
                    state,
                    Head {
                        owner: if fenced { head.owner } else { Some(txn.actor) },
                        fence: if fenced { head.fence } else { fence },
                        revision: expected + 1,
                        expires: self.now + LEASE.ttl_ms,
                    },
                );
                txn.overlay.payloads.insert(state, refs);
                txn.landed = true;
                self.lock(txn, &[Row::Owner(state)]);
            }
        }
        Ran::Next
    }

    /// GROUP_REVIVE_PG / REVIVE_ONE: clear the orphan stamp of every staged record.
    fn revive(&mut self, txn: &mut Txn) -> Ran {
        let Kind::Commit {
            state,
            fence,
            expected,
            ref sent,
            ..
        } = txn.kind
        else {
            unreachable!("revive runs only for a commit");
        };
        if !self.gated(txn, state, fence, expected + 1)
            || self.mutation == DurableMutation::CommitSkipsRevive
        {
            return Ran::Next;
        }
        let stamped: Vec<u64> = sent
            .iter()
            .copied()
            .filter(|key| {
                self.record(txn, *key)
                    .is_some_and(|record| record.orphaned.is_some())
            })
            .collect();
        let rows: Vec<Row> = stamped.iter().map(|key| Row::Record(*key)).collect();
        if self.blocked(txn.actor, &rows) {
            return Ran::Wait;
        }
        for key in &stamped {
            let record = self.record(txn, *key).expect("a stamped record is visible");
            txn.overlay.records.insert(
                *key,
                Some(Record {
                    orphaned: None,
                    ..record
                }),
            );
            self.stats.revived += 1;
        }
        self.lock(txn, &rows);
        Ran::Next
    }

    /// GROUP_RECORDS_PG / RECORD_ONE: insert each staged record, doing nothing on a conflict.
    fn insert_records(&mut self, txn: &mut Txn) -> Ran {
        let Kind::Commit {
            state,
            fence,
            expected,
            ref sent,
            ..
        } = txn.kind
        else {
            unreachable!("insert_records runs only for a commit");
        };
        if !self.gated(txn, state, fence, expected + 1) {
            return Ran::Next;
        }
        let sent: Vec<u64> = sent.iter().copied().collect();
        let rows: Vec<Row> = sent.iter().map(|key| Row::Record(*key)).collect();
        if self.blocked(txn.actor, &rows) {
            return Ran::Wait;
        }
        let mut inserted = Vec::new();
        for key in sent {
            if self.record(txn, key).is_none() {
                txn.overlay.records.insert(
                    key,
                    Some(Record {
                        holder: Some(state),
                        orphaned: None,
                    }),
                );
                inserted.push(Row::Record(key));
            }
        }
        self.lock(txn, &inserted);
        Ran::Next
    }

    /// GROUP_HOLDS_PG / HOLD_ONE: reference every staged record another state holds.
    fn holds(&mut self, txn: &mut Txn) -> Ran {
        let Kind::Commit {
            state,
            fence,
            expected,
            ref sent,
            ..
        } = txn.kind
        else {
            unreachable!("holds runs only for a commit");
        };
        if !self.gated(txn, state, fence, expected + 1) {
            return Ran::Done;
        }
        let wanted: Vec<u64> = sent
            .iter()
            .copied()
            .filter(|key| {
                self.record(txn, *key)
                    .is_some_and(|record| record.holder != Some(state))
                    && !self.has_ref(txn, *key, state)
            })
            .collect();
        let rows: Vec<Row> = wanted.iter().map(|key| Row::Ref(*key, state)).collect();
        if self.blocked(txn.actor, &rows) {
            return Ran::Wait;
        }
        for key in wanted {
            txn.overlay.refs.insert((key, state), true);
        }
        self.lock(txn, &rows);
        Ran::Done
    }

    /// COLLECT_HEAD: the owner's compare-and-set at its epoch and revision, renewing its lease.
    fn collect_head(&mut self, txn: &mut Txn) -> Ran {
        let Kind::Collect {
            state,
            fence,
            revision,
            ..
        } = txn.kind
        else {
            unreachable!("collect_head runs only for a collect");
        };
        if self.blocked(txn.actor, &[Row::Owner(state)]) {
            return Ran::Wait;
        }
        if let Some(head) = self.head(txn, state)
            && head.owner == Some(txn.actor)
            && head.fence == fence
            && head.revision == revision
        {
            let fence = if self.mutation == DurableMutation::CollectFencesOwner {
                head.fence + 1
            } else {
                head.fence
            };
            txn.overlay.heads.insert(
                state,
                Head {
                    fence,
                    expires: self.now + LEASE.ttl_ms,
                    ..head
                },
            );
            txn.landed = true;
            self.lock(txn, &[Row::Owner(state)]);
        }
        Ran::Next
    }

    /// COLLECT_REFS, COLLECT_HELD and COLLECT_STAMP, one per statement.
    fn collect_rows(&mut self, txn: &mut Txn, part: usize) -> Ran {
        let Kind::Collect {
            state, ref dead, ..
        } = txn.kind
        else {
            unreachable!("collect_rows runs only for a collect");
        };
        if !txn.landed {
            return Ran::Done;
        }
        let dead: Vec<u64> = dead.iter().copied().collect();
        let rows: Vec<Row> = match part {
            0 => dead
                .iter()
                .filter(|key| self.has_ref(txn, **key, state))
                .map(|key| Row::Ref(*key, state))
                .collect(),
            1 => dead
                .iter()
                .filter(|key| {
                    self.record(txn, **key)
                        .is_some_and(|record| record.holder == Some(state))
                })
                .map(|key| Row::Record(*key))
                .collect(),
            _ => dead
                .iter()
                .filter(|key| {
                    self.record(txn, **key)
                        .is_some_and(|record| record.holder.is_none() && record.orphaned.is_none())
                        && !self.referenced(txn, **key)
                })
                .map(|key| Row::Record(*key))
                .collect(),
        };
        if self.blocked(txn.actor, &rows) {
            return Ran::Wait;
        }
        for row in &rows {
            match (part, row) {
                (0, Row::Ref(key, state)) => {
                    txn.overlay.refs.insert((*key, *state), false);
                }
                (1, Row::Record(key)) => {
                    let record = self.record(txn, *key).expect("a held record is visible");
                    txn.overlay.records.insert(
                        *key,
                        Some(Record {
                            holder: None,
                            ..record
                        }),
                    );
                }
                (_, Row::Record(key)) => {
                    let record = self.record(txn, *key).expect("an orphan is visible");
                    txn.overlay.records.insert(
                        *key,
                        Some(Record {
                            orphaned: Some(self.now),
                            ..record
                        }),
                    );
                    self.stats.stamped += 1;
                }
                _ => unreachable!("collect rows are refs or records"),
            }
        }
        self.lock(txn, &rows);
        if part == 2 { Ran::Done } else { Ran::Next }
    }

    /// PURGE: delete records orphaned longer than the grace, unheld and unreferenced.
    fn purge(&mut self, txn: &mut Txn) -> Ran {
        let now = self.now;
        let grace = if self.mutation == DurableMutation::PurgeSkipsGrace {
            0
        } else {
            PURGE_GRACE_MS
        };
        let doomed: Vec<u64> = self
            .tables
            .records
            .iter()
            .filter(|(key, record)| {
                record.holder.is_none()
                    && record.orphaned.is_some_and(|at| {
                        if grace == 0 {
                            at <= now
                        } else {
                            at + grace < now
                        }
                    })
                    && !self.referenced(txn, **key)
            })
            .map(|(key, _)| *key)
            .collect();
        let rows: Vec<Row> = doomed.iter().map(|key| Row::Record(*key)).collect();
        if self.blocked(txn.actor, &rows) {
            return Ran::Wait;
        }
        for key in doomed {
            txn.overlay.records.insert(key, None);
            self.stats.purged += 1;
        }
        self.lock(txn, &rows);
        Ran::Done
    }

    fn release(&mut self, txn: &mut Txn, state: usize, fence: u64) -> Ran {
        if self.blocked(txn.actor, &[Row::Owner(state)]) {
            return Ran::Wait;
        }
        if let Some(head) = self.head(txn, state)
            && head.owner == Some(txn.actor)
            && head.fence == fence
        {
            txn.overlay.heads.insert(
                state,
                Head {
                    owner: None,
                    ..head
                },
            );
            txn.landed = true;
            self.lock(txn, &[Row::Owner(state)]);
        }
        Ran::Done
    }

    fn run(&mut self, txn: &mut Txn) -> Ran {
        match (txn.kind.clone(), txn.statement) {
            (Kind::Claim { state }, _) => self.claim(txn, state),
            (Kind::Commit { .. }, 0) => self.commit_head(txn),
            (Kind::Commit { .. }, 1) => self.revive(txn),
            (Kind::Commit { .. }, 2) => self.insert_records(txn),
            (Kind::Commit { .. }, _) => self.holds(txn),
            (Kind::Collect { .. }, 0) => self.collect_head(txn),
            (Kind::Collect { .. }, part) => self.collect_rows(txn, part - 1),
            (Kind::Release { state, fence }, _) => self.release(txn, state, fence),
            (Kind::Purge, _) => self.purge(txn),
        }
    }

    fn unlock(&mut self, txn: &Txn) {
        for row in &txn.locks {
            if self.locks.get(row) == Some(&txn.actor) {
                self.locks.remove(row);
            }
        }
    }

    fn commit(&mut self, txn: Txn) {
        self.unlock(&txn);
        let before = self.tables.clone();
        for (state, head) in &txn.overlay.heads {
            self.tables.heads.insert(*state, *head);
        }
        for (state, refs) in &txn.overlay.payloads {
            self.tables.payloads.insert(*state, refs.clone());
        }
        for (key, record) in &txn.overlay.records {
            match record {
                Some(record) => self.tables.records.insert(*key, *record),
                None => self.tables.records.remove(key),
            };
        }
        for (row, present) in &txn.overlay.refs {
            if *present {
                self.tables.refs.insert(*row);
            } else {
                self.tables.refs.remove(row);
            }
        }
        self.judge_commit(&txn, &before);
        self.answer(txn, &before);
        self.judge_tables();
    }

    /// I4 (a claim never takes a live lease), I5 (only the claimed owner's write lands) and
    /// GC never fencing an owner, judged on what the transaction changed.
    fn judge_commit(&mut self, txn: &Txn, before: &Tables) {
        if self.wrong.is_some() || !txn.landed {
            return;
        }
        match txn.kind {
            Kind::Claim { state } => {
                if let Some(head) = before.heads.get(&state)
                    && let Some(holder) = head.owner
                    && holder != txn.actor
                    && head.expires + LEASE.grace_ms >= self.now
                {
                    self.wrong = Some(format!(
                        "I4: owner {} claimed s{state} at {} while owner {holder} held it until {} (+{} grace)",
                        txn.actor, self.now, head.expires, LEASE.grace_ms
                    ));
                }
                let head = self.tables.heads[&state];
                self.claimed.insert(state, (txn.actor, head.fence));
            }
            Kind::Commit { state, fence, .. } => {
                if self.claimed.get(&state) != Some(&(txn.actor, fence)) {
                    self.wrong = Some(format!(
                        "I5: owner {} landed a write to s{state} at fence {fence}, but {:?} holds it",
                        txn.actor,
                        self.claimed.get(&state)
                    ));
                }
            }
            Kind::Collect { state, .. } => {
                if before.heads.get(&state).map(|head| head.fence)
                    != self.tables.heads.get(&state).map(|head| head.fence)
                {
                    self.wrong = Some(format!("GC fenced s{state}: a collect moved its fence"));
                }
            }
            Kind::Release { .. } | Kind::Purge => {}
        }
    }

    /// GC: every record a committed state references exists and that state holds it.
    fn judge_tables(&mut self) {
        if self.wrong.is_some() {
            return;
        }
        for (state, refs) in &self.tables.payloads {
            for key in refs {
                let held = self
                    .tables
                    .records
                    .get(key)
                    .is_some_and(|record| record.holder == Some(*state))
                    || self.tables.refs.contains(&(*key, *state));
                if !self.tables.records.contains_key(key) {
                    self.wrong = Some(format!(
                        "GC deleted {} which committed s{state} references",
                        key_name(*key)
                    ));
                    return;
                }
                if !held {
                    self.wrong = Some(format!(
                        "committed s{state} references {} without holding it",
                        key_name(*key)
                    ));
                    return;
                }
            }
        }
    }

    fn load(&self, state: usize, fence: u64) -> Belief {
        let head = self.tables.heads[&state];
        let held = self
            .tables
            .records
            .iter()
            .filter(|(_, record)| record.holder == Some(state))
            .map(|(key, _)| *key)
            .chain(
                self.tables
                    .refs
                    .iter()
                    .filter(|(_, holder)| *holder == state)
                    .map(|(key, _)| *key),
            )
            .collect();
        Belief {
            fence,
            revision: head.revision,
            refs: self
                .tables
                .payloads
                .get(&state)
                .cloned()
                .unwrap_or_default(),
            held,
        }
    }

    fn answer(&mut self, txn: Txn, before: &Tables) {
        let actor = txn.actor;
        match txn.kind {
            Kind::Claim { state } if txn.landed => {
                let head = self.tables.heads[&state];
                match before.heads.get(&state).and_then(|head| head.owner) {
                    None if before.heads.contains_key(&state) => self.stats.takeovers_released += 1,
                    Some(holder) if holder != actor => self.stats.takeovers_expired += 1,
                    _ => {}
                }
                self.stats.claimed += 1;
                let belief = self.load(state, head.fence);
                self.actors[actor].beliefs.insert(state, belief);
            }
            Kind::Claim { .. } => self.stats.claims_busy += 1,
            Kind::Commit {
                state, refs, sent, ..
            } if txn.landed => {
                self.stats.commits += 1;
                if let Some(belief) = self.actors[actor].beliefs.get_mut(&state) {
                    belief.revision += 1;
                    belief.refs = refs;
                    belief.held.extend(sent);
                }
            }
            Kind::Commit { state, fence, .. } | Kind::Collect { state, fence, .. } => {
                let head = self.tables.heads[&state];
                let commit = matches!(txn.kind, Kind::Commit { .. });
                if commit {
                    self.stats.commits_fenced += 1;
                } else if txn.landed {
                    self.stats.collects += 1;
                    if let (Some(belief), Kind::Collect { dead, .. }) =
                        (self.actors[actor].beliefs.get_mut(&state), &txn.kind)
                    {
                        belief.held.retain(|key| !dead.contains(key));
                    }
                    return;
                } else {
                    self.stats.collects_refused += 1;
                }
                if head.owner == Some(actor) && head.fence == fence {
                    let belief = self.load(state, fence);
                    self.actors[actor].beliefs.insert(state, belief);
                } else {
                    self.actors[actor].beliefs.remove(&state);
                }
            }
            Kind::Release { state, .. } => {
                if txn.landed {
                    self.stats.releases += 1;
                }
                self.actors[actor].beliefs.remove(&state);
            }
            Kind::Purge => {}
        }
    }

    fn abort(&mut self, txn: Txn) {
        self.unlock(&txn);
        self.stats.timeouts += 1;
        if let Kind::Commit { state, .. } | Kind::Collect { state, .. } = txn.kind {
            self.actors[txn.actor].beliefs.remove(&state);
        }
    }

    fn plan(&mut self, actor: usize) -> Option<Kind> {
        if actor == self.actors.len() - 1 {
            return self.rng.chance(PURGE_PPM).then_some(Kind::Purge);
        }
        if self.actors[actor].stalled_until > self.now {
            return None;
        }
        let owned: Vec<usize> = self.actors[actor].beliefs.keys().copied().collect();
        let state = if !owned.is_empty() && self.rng.chance(OWNED_PPM) {
            owned[self.rng.pick(owned.len())]
        } else {
            self.rng.pick(STATES)
        };
        let Some(belief) = self.actors[actor].beliefs.get(&state).cloned() else {
            return Some(Kind::Claim { state });
        };
        let roll = self.rng.below(1_000_000);
        if roll < COMMIT_PPM {
            let mut refs = belief.refs.clone();
            for _ in 0..self.rng.below(2) {
                if let Some(key) = refs.iter().nth(self.rng.pick(refs.len().max(1))).copied() {
                    refs.remove(&key);
                }
            }
            while (refs.len() as u64) < self.rng.range(1, REFS_MAX + 1) {
                let key = if self.rng.chance(SHARED_PPM) {
                    self.rng.below(SHARED_KEYS)
                } else {
                    (state as u64 + 1) * 1_000 + self.rng.below(OWN_KEYS)
                };
                refs.insert(key);
            }
            let sent = refs.difference(&belief.held).copied().collect();
            return Some(Kind::Commit {
                state,
                fence: belief.fence,
                expected: belief.revision,
                refs,
                sent,
            });
        }
        if roll < COMMIT_PPM + COLLECT_PPM {
            let dead: BTreeSet<u64> = belief.held.difference(&belief.refs).copied().collect();
            return (!dead.is_empty()).then_some(Kind::Collect {
                state,
                fence: belief.fence,
                revision: belief.revision,
                dead,
            });
        }
        if roll < COMMIT_PPM + COLLECT_PPM + RELEASE_PPM {
            return Some(Kind::Release {
                state,
                fence: belief.fence,
            });
        }
        if roll < COMMIT_PPM + COLLECT_PPM + RELEASE_PPM + CRASH_PPM {
            self.stats.crashes += 1;
            self.actors[actor].beliefs.clear();
            return None;
        }
        self.stats.stalls += 1;
        self.actors[actor].stalled_until = self.now + self.rng.range(1, STALL_MS_MAX);
        None
    }

    fn step(&mut self) {
        self.stats.steps += 1;
        self.now += self.rng.below(TICK_MS_MAX);
        let actor = self.rng.pick(self.actors.len());
        let mut txn = match self.open.remove(&actor) {
            Some(txn) => txn,
            None => match self.plan(actor) {
                Some(kind) => Txn {
                    actor,
                    kind,
                    statement: 0,
                    began: self.now,
                    landed: false,
                    overlay: Overlay::default(),
                    locks: BTreeSet::new(),
                },
                None => return,
            },
        };
        if self.now > txn.began + STATEMENT_MS {
            self.abort(txn);
            return;
        }
        match self.run(&mut txn) {
            Ran::Next => {
                txn.statement += 1;
                self.open.insert(actor, txn);
            }
            Ran::Wait => {
                self.stats.lock_waits += 1;
                self.open.insert(actor, txn);
            }
            Ran::Done => self.commit(txn),
        }
    }

    fn digest(&self) -> u64 {
        let mut hash = FNV_OFFSET;
        for (state, head) in &self.tables.heads {
            hash = fnv(
                hash,
                format!(
                    "{state}:{:?}:{}:{}:{}",
                    head.owner, head.fence, head.revision, head.expires
                )
                .as_bytes(),
            );
        }
        for (key, record) in &self.tables.records {
            hash = fnv(
                hash,
                format!("{key}:{:?}:{:?}", record.holder, record.orphaned).as_bytes(),
            );
        }
        for (key, state) in &self.tables.refs {
            hash = fnv(hash, format!("{key}>{state}").as_bytes());
        }
        fnv(hash, self.stats.summary().as_bytes())
    }
}

struct Outcome {
    digest: u64,
    shape: String,
    wrong: Option<String>,
    stats: DurableStats,
}

fn run_seed(seed: u64, mutation: DurableMutation) -> Outcome {
    let mut rng = Rng::derive(seed, "durable");
    let owners = rng.range(OWNERS_MIN, OWNERS_MAX + 1) as usize;
    let steps = rng.range(STEPS_MIN, STEPS_MAX + 1);
    let mut world = World {
        rng,
        mutation,
        now: START_MS,
        tables: Tables::default(),
        claimed: BTreeMap::new(),
        actors: (0..=owners).map(|_| Actor::default()).collect(),
        open: BTreeMap::new(),
        locks: BTreeMap::new(),
        stats: DurableStats {
            seeds: 1,
            ..DurableStats::default()
        },
        wrong: None,
    };
    for _ in 0..steps {
        world.step();
        if world.wrong.is_some() {
            break;
        }
    }
    Outcome {
        digest: world.digest(),
        shape: format!("owners={owners} steps={steps}"),
        wrong: world.wrong,
        stats: world.stats,
    }
}

pub fn judge(
    seed: u64,
    mutation: DurableMutation,
    twice: bool,
    verbose: bool,
    totals: &mut DurableStats,
) -> Option<String> {
    let first = run_seed(seed, mutation);
    totals.add(&first.stats);
    if verbose {
        println!(
            "seed {seed} digest {:016x} {} stats={}",
            first.digest,
            first.shape,
            first.stats.summary()
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

pub fn replay(seed: u64, mutation: DurableMutation) -> String {
    let mut command = format!("nanocode-sim --scenario durable --seed {seed}");
    if mutation != DurableMutation::None {
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
    mutation: DurableMutation,
    totals: &DurableStats,
) -> ExitCode {
    println!(
        "RESULT seeds={}..{} scenario=durable mutation={} passed={passed} failed={failed} twice={twice} stats={}",
        seeds.start,
        seeds.end,
        mutation.name(),
        totals.summary()
    );
    let uncovered = totals.uncovered();
    match mutation {
        DurableMutation::None if failed == 0 && (totals.seeds < 64 || uncovered.is_empty()) => {
            ExitCode::SUCCESS
        }
        DurableMutation::None => {
            if !uncovered.is_empty() {
                println!(
                    "COVERAGE durable transitions never taken: {}",
                    uncovered.join(",")
                );
            }
            ExitCode::FAILURE
        }
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
