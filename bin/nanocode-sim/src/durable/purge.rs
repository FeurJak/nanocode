use std::{
    future::Future,
    pin::pin,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    task::{Context, Poll, Waker},
    time::Duration,
};

use nanocode_durability_core::{
    Purged, Sql, SqlDialect, SqlRow, SqlStatement, SqlStore, StoreError, StoreFuture,
    sim::{self as durability_sim, Mutation as DurabilityRule},
};
use nanocode_sync::Atom;

use super::{DurableMutation, DurableStats};
use crate::{executor::Clock, rng::Rng};

const RECORDS_MIN: u64 = 8;
const RECORDS_MAX: u64 = 96;
const ROUNDS_MIN: u64 = 2;
const ROUNDS_MAX: u64 = 6;
const BATCH_MIN: u64 = 1;
const BATCH_MAX: u64 = 12;
const BUDGET_MS_MIN: u64 = 20;
const BUDGET_MS_MAX: u64 = 600;
const GRACE_MS: u64 = 300;
const ORPHANED_MS_MAX: u64 = 4 * GRACE_MS;
const ROW_MS_MAX: u64 = 4;
const STALL_PPM: u64 = 60_000;
const STALL_MS_MAX: u64 = 4 * BUDGET_MS_MAX;
const HELD_PPM: u64 = 200_000;
const REFERENCED_PPM: u64 = 200_000;
const ORPHANED_PPM: u64 = 600_000;
const IDLE_MS_MAX: u64 = 2 * GRACE_MS;
const START_MS: u64 = 1_000_000;
const POLLS_MAX: u64 = 100_000;
const NANOS_PER_MS: u64 = 1_000_000;
const DEADLINE: &str = "SELECT set_config('statement_timeout'";
const PURGE: &str = "DELETE FROM records USING (SELECT picked.id";

#[derive(Clone, Copy, Debug)]
struct Record {
    orphaned: Option<u64>,
    held: bool,
    referenced: bool,
}

impl Record {
    fn purgeable(self, now_ms: u64) -> bool {
        !self.held && !self.referenced && self.orphaned.is_some_and(|at| at + GRACE_MS < now_ms)
    }
}

struct PurgeHost {
    clock: Arc<Clock>,
    rng: Atom<Rng>,
    records: Atom<std::collections::BTreeMap<u64, Record>>,
    batch: AtomicU64,
    widest: AtomicU64,
    batches: AtomicU64,
    cuts: AtomicU64,
}

impl PurgeHost {
    fn now_ms(&self) -> u64 {
        START_MS + self.clock.now_ns() / NANOS_PER_MS
    }

    fn draw(&self, low: u64, high: u64) -> u64 {
        self.rng.update(|rng| rng.range(low, high))
    }

    fn chance(&self, ppm: u64) -> bool {
        self.rng.update(|rng| rng.chance(ppm))
    }

    async fn execute(
        &self,
        statements: &[SqlStatement<'_>],
    ) -> Result<Vec<Vec<SqlRow>>, StoreError> {
        let mut deadline_ms = None;
        let mut results = Vec::with_capacity(statements.len());
        for statement in statements {
            if statement.sql.starts_with(DEADLINE) {
                let ms: u64 = statement.params[0]
                    .parse()
                    .expect("a statement deadline is in ms");
                assert!(ms > 0, "a statement deadline of 0 turns the bound off");
                deadline_ms = Some(ms);
                results.push(vec![vec![Some(ms.to_string())]]);
                continue;
            }
            assert!(
                statement.sql.starts_with(PURGE),
                "the purge check runs only purge statements: {}",
                statement.sql
            );
            let grace: u64 = statement.params[0].parse().expect("a purge grace is in ms");
            let limit: u64 = statement.params[1]
                .parse()
                .expect("a purge limit is a count");
            assert_eq!(grace, GRACE_MS, "a purge passes its caller's grace");
            let now_ms = self.now_ms();
            let picked: Vec<u64> = self
                .records
                .get()
                .iter()
                .filter(|(_, record)| record.purgeable(now_ms))
                .map(|(id, _)| *id)
                .take(usize::try_from(limit).unwrap_or(usize::MAX))
                .collect();
            let mut cost_ms = picked.len() as u64 * self.draw(1, ROW_MS_MAX + 1);
            if self.chance(STALL_PPM) {
                cost_ms += self.draw(1, STALL_MS_MAX);
            }
            if let Some(deadline_ms) = deadline_ms
                && cost_ms > deadline_ms
            {
                self.clock.sleep(Duration::from_millis(deadline_ms)).await;
                self.cuts.fetch_add(1, Ordering::Relaxed);
                return Err(StoreError::Busy);
            }
            self.clock.sleep(Duration::from_millis(cost_ms)).await;
            self.records.update(|records| {
                for id in &picked {
                    records.remove(id);
                }
            });
            self.batches.fetch_add(1, Ordering::Relaxed);
            self.widest
                .fetch_max(picked.len() as u64, Ordering::Relaxed);
            results.push(picked.iter().map(|id| vec![Some(id.to_string())]).collect());
        }
        Ok(results)
    }
}

impl Sql for PurgeHost {
    fn call_bound(&self) -> Option<Duration> {
        None
    }

    fn transaction<'a>(
        &'a self,
        _: &'static str,
        statements: &'a [SqlStatement<'a>],
    ) -> StoreFuture<'a, Result<Vec<Vec<SqlRow>>, StoreError>> {
        Box::pin(self.execute(statements))
    }

    fn dialect(&self) -> SqlDialect {
        SqlDialect::Postgres
    }
}

fn drive<T>(clock: &Clock, future: impl Future<Output = T>) -> Result<T, String> {
    let mut future = pin!(future);
    let mut context = Context::from_waker(Waker::noop());
    for _ in 0..POLLS_MAX {
        if let Poll::Ready(output) = future.as_mut().poll(&mut context) {
            return Ok(output);
        }
        if !clock.advance_to_next() {
            return Err("a purge waits with no timer to wake it".to_owned());
        }
    }
    Err(format!("a purge did not finish in {POLLS_MAX} polls"))
}

const fn rule(mutation: DurableMutation) -> DurabilityRule {
    match mutation {
        DurableMutation::PurgeUnbatched => DurabilityRule::PurgeUnbatched,
        DurableMutation::PurgeSkipsDeadline => DurabilityRule::PurgeSkipsDeadline,
        DurableMutation::PurgeFinishesEarly => DurabilityRule::PurgeFinishesEarly,
        _ => DurabilityRule::None,
    }
}

fn seeded(rng: &mut Rng, host: &PurgeHost, first: u64, count: u64) {
    let now_ms = host.now_ms();
    host.records.update(|records| {
        for id in first..first + count {
            let orphaned = rng
                .chance(ORPHANED_PPM)
                .then(|| now_ms.saturating_sub(rng.below(ORPHANED_MS_MAX)));
            records.insert(
                id,
                Record {
                    orphaned,
                    held: orphaned.is_none() && rng.chance(HELD_PPM),
                    referenced: rng.chance(REFERENCED_PPM),
                },
            );
        }
    });
}

pub(super) fn run(
    seed: u64,
    mutation: DurableMutation,
    stats: &mut DurableStats,
) -> Result<(), String> {
    let clock = Clock::new();
    let mut rng = Rng::derive(seed, "durable-purge");
    let host = Arc::new(PurgeHost {
        clock: Arc::clone(&clock),
        rng: Atom::new(Rng::derive(seed, "durable-purge-host")),
        records: Atom::new(std::collections::BTreeMap::new()),
        batch: AtomicU64::new(0),
        widest: AtomicU64::new(0),
        batches: AtomicU64::new(0),
        cuts: AtomicU64::new(0),
    });
    let mut store = SqlStore::shared(Arc::clone(&host)).migrated();
    durability_sim::mutate(rule(mutation));
    let checked = crate::host::entered(seed, &clock, || {
        let mut next = 0;
        for round in 0..rng.range(ROUNDS_MIN, ROUNDS_MAX + 1) {
            let count = rng.range(RECORDS_MIN, RECORDS_MAX + 1);
            seeded(&mut rng, &host, next, count);
            next += count;
            let batch = rng.range(BATCH_MIN, BATCH_MAX + 1);
            let budget = Duration::from_millis(rng.range(BUDGET_MS_MIN, BUDGET_MS_MAX + 1));
            host.batch.store(batch, Ordering::Relaxed);
            host.widest.store(0, Ordering::Relaxed);
            let started_ms = host.now_ms();
            let before = host.records.get().len() as u64;
            let answer = drive(&clock, store.purge_within(GRACE_MS, batch as usize, budget))?;
            let elapsed_ms = host.now_ms() - started_ms;
            let deleted = before - host.records.get().len() as u64;
            let widest = host.widest.load(Ordering::Relaxed);
            if widest > batch {
                return Err(format!(
                    "round {round}: one purge transaction deleted {widest} records with a batch of {batch}"
                ));
            }
            let budget_ms = budget.as_millis() as u64;
            if elapsed_ms > budget_ms {
                return Err(format!(
                    "round {round}: a purge with a budget of {budget_ms} ms returned after {elapsed_ms} ms"
                ));
            }
            match answer {
                Ok(Purged {
                    deleted: counted,
                    finished,
                    ..
                }) => {
                    if counted != deleted {
                        return Err(format!(
                            "round {round}: a purge reported {counted} deletions of {deleted}"
                        ));
                    }
                    if finished {
                        let left = host
                            .records
                            .get()
                            .values()
                            .filter(|record| record.purgeable(started_ms))
                            .count();
                        if left > 0 {
                            return Err(format!(
                                "round {round}: a finished purge left {left} records past their grace"
                            ));
                        }
                        stats.purges_finished += 1;
                    }
                }
                Err(StoreError::Busy) => stats.purges_cut += 1,
                Err(error) => return Err(format!("round {round}: a purge failed: {error}")),
            }
            let idle = Duration::from_millis(rng.below(IDLE_MS_MAX) + 1);
            drive(&clock, clock.sleep(idle))?;
        }
        Ok(())
    });
    durability_sim::mutate(DurabilityRule::None);
    stats.purge_batches += host.batches.load(Ordering::Relaxed);
    checked
}
