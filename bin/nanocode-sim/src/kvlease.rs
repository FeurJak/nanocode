use std::{collections::BTreeMap, ops::Range, process::ExitCode, sync::Arc};

use nanocode_durability_core::{
    KeyValue, KeyValueStore, Lease, OwnerId, OwnerToken, StateStore, StoreError, StoreFuture,
    Versioned,
    sim::{self as kv_sim, Mutation as Rule},
};
use nanocode_sync::{Atom, block_on};

use crate::rng::{FNV_OFFSET, Rng, fnv};

const STATES: u64 = 2;
const OWNERS: Range<u64> = 2..6;
const STEPS: Range<u64> = 200..800;
const TTL_MS: u64 = 1_000;
const GRACE_MS: u64 = 300;
const LEASE: Lease = Lease {
    ttl_ms: TTL_MS,
    grace_ms: GRACE_MS,
};
const TICK_MS_MAX: u64 = 120;
const START_MS: u64 = 1_000_000;
const COMMIT_PPM: u64 = 600_000;
const CRASH_PPM: u64 = 30_000;
const HEADS: &str = "state/";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KvLeaseMutation {
    None,
    ClaimStealsLive,
    ClaimSkipsGrace,
}

impl KvLeaseMutation {
    const ALL: [Self; 3] = [Self::None, Self::ClaimStealsLive, Self::ClaimSkipsGrace];

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|mutation| mutation.name() == name)
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::ClaimStealsLive => "claim_steals_live",
            Self::ClaimSkipsGrace => "claim_skips_grace",
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

    const fn rule(self) -> Rule {
        match self {
            Self::None => Rule::None,
            Self::ClaimStealsLive => Rule::KvClaimStealsLive,
            Self::ClaimSkipsGrace => Rule::KvClaimSkipsGrace,
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct KvLeaseStats {
    pub seeds: u64,
    pub steps: u64,
    pub claimed: u64,
    pub claims_busy: u64,
    pub takeovers: u64,
    pub commits: u64,
    pub commits_fenced: u64,
    pub crashes: u64,
}

impl KvLeaseStats {
    pub const fn add(&mut self, other: &Self) {
        self.seeds += other.seeds;
        self.steps += other.steps;
        self.claimed += other.claimed;
        self.claims_busy += other.claims_busy;
        self.takeovers += other.takeovers;
        self.commits += other.commits;
        self.commits_fenced += other.commits_fenced;
        self.crashes += other.crashes;
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Seen {
    fence: u64,
    expires_ms: Option<u64>,
    owner: String,
}

fn seen(version: &str) -> Option<Seen> {
    let (leased, rest) = match version.strip_prefix('L') {
        Some(rest) => (true, rest),
        None => (false, version),
    };
    let mut parts = rest.splitn(if leased { 4 } else { 3 }, '.');
    let fence = parts.next()?.parse().ok()?;
    let _revision: u64 = parts.next()?.parse().ok()?;
    let expires_ms = match leased {
        true => Some(parts.next()?.parse().ok()?),
        false => None,
    };
    Some(Seen {
        fence,
        expires_ms,
        owner: parts.next()?.to_owned(),
    })
}

fn live(head: &Seen, now_ms: u64) -> bool {
    head.expires_ms
        .is_some_and(|expires_ms| expires_ms + GRACE_MS >= now_ms)
}

#[derive(Clone, Debug, Default)]
struct Kv {
    entries: BTreeMap<String, Versioned>,
    now_ms: u64,
    wrong: Option<String>,
    takeovers: u64,
}

struct ClockKv {
    kv: Atom<Kv>,
}

impl ClockKv {
    fn swap(&self, key: &str, expected: Option<&str>, version: &str, value: &str) -> bool {
        self.kv.update(|kv| {
            let current = kv.entries.get(key).map(|entry| entry.version.as_str());
            if current != expected {
                return false;
            }
            if key.starts_with(HEADS)
                && let Some(old) = current.and_then(seen)
            {
                match seen(version) {
                    Some(new) if new.owner != old.owner || new.fence != old.fence => {
                        if new.fence <= old.fence {
                            kv.wrong.get_or_insert(format!(
                                "I1: {key} claim at fence {} over fence {}",
                                new.fence, old.fence
                            ));
                        }
                        if new.owner != old.owner && live(&old, kv.now_ms) {
                            kv.wrong.get_or_insert(format!(
                                "I4: {} took {key} from {} whose lease runs to {:?} + grace {GRACE_MS} at {}",
                                new.owner, old.owner, old.expires_ms, kv.now_ms
                            ));
                        }
                        if new.owner != old.owner {
                            kv.takeovers += 1;
                        }
                    }
                    Some(_) => {}
                    None => {
                        kv.wrong
                            .get_or_insert(format!("{key} written with version {version}"));
                    }
                }
            }
            kv.entries.insert(
                key.to_owned(),
                Versioned {
                    version: version.to_owned(),
                    value: value.to_owned(),
                },
            );
            true
        })
    }
}

impl KeyValue for ClockKv {
    fn get<'a>(&'a self, key: &'a str) -> StoreFuture<'a, Result<Option<Versioned>, StoreError>> {
        let entry = self.kv.read(|kv| kv.entries.get(key).cloned());
        Box::pin(async move { Ok(entry) })
    }

    fn put_values<'a>(
        &'a self,
        entries: &'a [(String, String)],
    ) -> StoreFuture<'a, Result<(), StoreError>> {
        self.kv.update(|kv| {
            for (key, value) in entries {
                kv.entries.insert(
                    key.clone(),
                    Versioned {
                        version: String::new(),
                        value: value.clone(),
                    },
                );
            }
        });
        Box::pin(async { Ok(()) })
    }

    fn compare_and_swap<'a>(
        &'a self,
        key: &'a str,
        expected_version: Option<&'a str>,
        version: &'a str,
        value: &'a str,
    ) -> StoreFuture<'a, Result<bool, StoreError>> {
        let swapped = self.swap(key, expected_version, version, value);
        Box::pin(async move { Ok(swapped) })
    }

    fn clock_ms(&self) -> StoreFuture<'_, Result<u64, StoreError>> {
        let now_ms = self.kv.read(|kv| kv.now_ms);
        Box::pin(async move { Ok(now_ms) })
    }
}

struct Owner {
    id: String,
    held: BTreeMap<String, (OwnerToken, u64)>,
}

struct Outcome {
    digest: u64,
    wrong: Option<String>,
    stats: KvLeaseStats,
    shape: String,
}

fn step(
    rng: &mut Rng,
    backend: &Arc<ClockKv>,
    store: &mut KeyValueStore<ClockKv>,
    owners: &mut [Owner],
    stats: &mut KvLeaseStats,
) -> Option<String> {
    stats.steps += 1;
    let tick = rng.below(TICK_MS_MAX + 1);
    backend.kv.update(|kv| kv.now_ms += tick);
    let owner = &mut owners[rng.pick(owners.len())];
    let state = format!("s{}", rng.below(STATES));
    if rng.chance(CRASH_PPM) {
        stats.crashes += 1;
        owner.held.clear();
        return None;
    }
    let held = owner.held.get(&state).cloned();
    match held {
        Some((token, revision)) if rng.chance(COMMIT_PPM) => {
            stats.commits += 1;
            match block_on(store.replace(&state, &token, revision, "payload", &[])) {
                Ok(next) => {
                    owner.held.insert(state, (token, next));
                }
                Err(StoreError::Fenced | StoreError::Conflict { .. }) => {
                    stats.commits_fenced += 1;
                    owner.held.remove(&state);
                }
                Err(error) => return Some(format!("commit of {state} failed: {error}")),
            }
        }
        Some(_) => {}
        None => {
            let before = backend.kv.read(|kv| {
                (
                    kv.entries
                        .get(&format!("{HEADS}{state}"))
                        .and_then(|entry| seen(&entry.version)),
                    kv.now_ms,
                )
            });
            match block_on(store.acquire(&state, OwnerId::from(owner.id.clone()))) {
                Ok(owned) => {
                    stats.claimed += 1;
                    owner
                        .held
                        .insert(state, (owned.owner, owned.state.revision));
                }
                Err(StoreError::Busy) => {
                    stats.claims_busy += 1;
                    let (head, now_ms) = before;
                    let refused_live = head
                        .as_ref()
                        .is_some_and(|head| head.owner != owner.id && live(head, now_ms));
                    if !refused_live {
                        return Some(format!(
                            "I4: {} refused Busy on {state} whose head {head:?} is not live at {now_ms}",
                            owner.id
                        ));
                    }
                }
                Err(error) => return Some(format!("claim of {state} failed: {error}")),
            }
        }
    }
    None
}

fn run_seed(seed: u64, mutation: KvLeaseMutation) -> Outcome {
    let mut rng = Rng::derive(seed, "kv_lease");
    let mut stats = KvLeaseStats {
        seeds: 1,
        ..KvLeaseStats::default()
    };
    let backend = Arc::new(ClockKv {
        kv: Atom::new(Kv {
            now_ms: START_MS,
            ..Kv::default()
        }),
    });
    let mut store = KeyValueStore::shared(Arc::clone(&backend)).leased(LEASE);
    let mut owners: Vec<Owner> = (0..rng.range(OWNERS.start, OWNERS.end))
        .map(|at| Owner {
            id: format!("owner-{at}"),
            held: BTreeMap::new(),
        })
        .collect();
    let steps = rng.range(STEPS.start, STEPS.end);
    let shape = format!("owners={} steps={steps}", owners.len());
    kv_sim::mutate(mutation.rule());
    let mut wrong = None;
    for _ in 0..steps {
        wrong = step(&mut rng, &backend, &mut store, &mut owners, &mut stats)
            .or_else(|| backend.kv.read(|kv| kv.wrong.clone()));
        if wrong.is_some() {
            break;
        }
    }
    kv_sim::mutate(Rule::None);
    let kv = backend.kv.get();
    stats.takeovers = kv.takeovers;
    let digest = kv.entries.iter().fold(
        fnv(FNV_OFFSET, &seed.to_le_bytes()),
        |hash, (key, entry)| fnv(fnv(hash, key.as_bytes()), entry.version.as_bytes()),
    );
    Outcome {
        digest,
        wrong,
        stats,
        shape,
    }
}

pub fn judge(
    seed: u64,
    mutation: KvLeaseMutation,
    twice: bool,
    verbose: bool,
    totals: &mut KvLeaseStats,
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

pub fn replay(seed: u64, mutation: KvLeaseMutation) -> String {
    let mut command = format!("nanocode-sim --scenario kv_lease --seed {seed}");
    if mutation != KvLeaseMutation::None {
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
    mutation: KvLeaseMutation,
    totals: &KvLeaseStats,
) -> ExitCode {
    println!(
        "RESULT seeds={}..{} scenario=kv_lease mutation={} passed={passed} failed={failed} twice={twice} stats={totals:?}",
        seeds.start,
        seeds.end,
        mutation.name()
    );
    match mutation {
        KvLeaseMutation::None if failed == 0 => ExitCode::SUCCESS,
        KvLeaseMutation::None => ExitCode::FAILURE,
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
