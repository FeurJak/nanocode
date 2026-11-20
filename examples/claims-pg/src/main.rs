//! Board claims on a real Postgres 16+: a claim+finish loop over the partitioned board (one
//! shards::Committer per partition) on PartedLog::fenced over SqlParts, with the NOTIFY sender
//! (SqlParts::notifier) running as it does in a node.
//!
//! examples/claims-pg/run.sh starts a private Postgres and runs this once per database:
//! cargo run --release -p nanocode-claims-pg -- postgres://nc@127.0.0.1:5544/claims parts=16
//!
//! Options are key=value: parts (board partitions, 16), log_parts (log partitions, = parts),
//! claimers (64), tasks (20000), batch (committer group, 256), conns (pool, 64), threads (4),
//! max_rows (rows per log commit, 512), pipeline (1: send each finish with the next claim rather
//! than before it, so both can share a log commit; 0). The board must be fresh: give each run its own database.
//! Prints one METRIC line: claims/s over the whole claim phase and over its first 90% (before the
//! tail where only some partitions still have tasks), claim latency, Postgres transactions per
//! claim, board writes per log append and log appends per Postgres commit.

use std::{
    collections::BTreeMap,
    num::NonZeroU32,
    sync::{
        Arc, Mutex, PoisonError,
        atomic::{AtomicU64, Ordering},
    },
    time::{Instant, SystemTime, UNIX_EPOCH},
};

use nanocode_board::{
    BoardError, ClaimId, NewTask, Op, Outcome, TaskId, Write,
    access::{Can, Grant, Identity, Verifier, Who},
    shards::{self, Committer},
};
use nanocode_durability_core::{Sql, SqlDialect, SqlRow, SqlStatement, StoreError, StoreFuture};
use nanocode_partitions::{Partition, partition_of};
use nanocode_store_sql::{PgOptions, PgPool, PgSql};
use nanocode_turnlog::{
    LogHost,
    parted::{PartedLog, SqlParts},
};

const OWNER: &str = "user:lead";
const SCOPE: &str = "board";
const LEASE_MS: u64 = 600_000;

/// Counts transactions, each one pipelined round trip on PgSql.
struct Counted {
    inner: PgSql,
    trips: AtomicU64,
}

impl Sql for Counted {
    fn dialect(&self) -> SqlDialect {
        self.inner.dialect()
    }

    fn transaction<'a>(
        &'a self,
        operation: &'static str,
        statements: &'a [SqlStatement<'a>],
    ) -> StoreFuture<'a, Result<Vec<Vec<SqlRow>>, StoreError>> {
        self.trips.fetch_add(1, Ordering::Relaxed);
        self.inner.transaction(operation, statements)
    }
}

struct Tokens;

impl Verifier for Tokens {
    fn verify(&self, credential: &str) -> Option<String> {
        credential.strip_prefix("token:").map(str::to_owned)
    }
}

struct Options {
    url: String,
    parts: u32,
    log_parts: u32,
    claimers: usize,
    tasks: usize,
    batch: usize,
    conns: usize,
    threads: usize,
    max_rows: usize,
    pipeline: bool,
}

impl Options {
    fn parse() -> Result<Self, String> {
        let mut url = "postgres://nc@127.0.0.1:5544/claims".to_owned();
        let mut values: BTreeMap<String, usize> = BTreeMap::new();
        for arg in std::env::args().skip(1) {
            match arg.split_once('=') {
                Some((key, value)) if !arg.starts_with("postgres") => {
                    let value = value.parse().map_err(|_| format!("bad number in {arg}"))?;
                    values.insert(key.to_owned(), value);
                }
                _ => url = arg,
            }
        }
        let known = [
            "parts",
            "log_parts",
            "claimers",
            "tasks",
            "batch",
            "conns",
            "threads",
            "max_rows",
            "pipeline",
        ];
        if let Some(key) = values.keys().find(|key| !known.contains(&key.as_str())) {
            return Err(format!("unknown option {key}"));
        }
        let get = |key: &str, default: usize| values.get(key).copied().unwrap_or(default);
        let parts = u32::try_from(get("parts", 16)).map_err(|_| "parts too large")?;
        Ok(Self {
            url,
            parts,
            log_parts: u32::try_from(get("log_parts", parts as usize))
                .map_err(|_| "log_parts too large")?,
            claimers: get("claimers", 64).max(1),
            tasks: get("tasks", 20_000),
            batch: get("batch", 256),
            conns: get("conns", 64),
            threads: get("threads", 4).max(1),
            max_rows: get("max_rows", 512),
            pipeline: get("pipeline", 0) != 0,
        })
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| {
            u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
        })
}

fn micros(since: Instant) -> u64 {
    u64::try_from(since.elapsed().as_micros()).unwrap_or(u64::MAX)
}

fn percentile(sorted: &[u64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let index = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[index] as f64 / 1000.0
}

fn write(by: &str, key: String, op: Op) -> Write {
    Write {
        by: Some(by.to_owned()),
        key: Some(key),
        at_ms: now_ms(),
        op,
    }
}

static FIRST_ERROR: Mutex<Option<String>> = Mutex::new(None);

fn note(error: &BoardError) {
    FIRST_ERROR
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .get_or_insert_with(|| format!("{error:?}"));
}

/// Owner writes every partition needs before anyone posts: the charter and an open grant.
async fn charter(committers: &[Committer]) -> Result<(), BoardError> {
    let everyone = Grant {
        who: vec![Who::Everyone],
        can: BTreeMap::from([(
            "*".to_owned(),
            vec![Can::Read, Can::Post, Can::Label, Can::Rank],
        )]),
    };
    for committer in committers {
        committer
            .submit(write(OWNER, "charter".to_owned(), Op::Charter {}))
            .await?;
        committer
            .submit(write(
                OWNER,
                "grant".to_owned(),
                Op::Grant {
                    grant: everyone.clone(),
                },
            ))
            .await?;
    }
    Ok(())
}

fn task(index: usize) -> NewTask {
    NewTask {
        id: TaskId(format!("t{index}")),
        parent: None,
        spec: format!("root {index}"),
        labels: BTreeMap::new(),
        needs: Vec::new(),
        terms: nanocode_board::tasks::Terms::default(),
    }
}

/// What one claimer measured.
#[derive(Default)]
struct Claimed {
    latencies: Vec<u64>,
    done_at: Vec<u64>,
    refused: u64,
}

async fn finish(committer: Committer, by: String, index: usize, claim: ClaimId) -> bool {
    let finish = Op::Finish {
        task: TaskId(format!("t{index}")),
        claim,
        outcome: Outcome::Done(String::new()),
    };
    match committer
        .submit(write(&by, format!("finish:{index}"), finish))
        .await
    {
        Ok(_) => true,
        Err(error) => {
            note(&error);
            false
        }
    }
}

async fn claimer(
    committer: Committer,
    by: String,
    tasks: Vec<usize>,
    started: Instant,
    pipeline: bool,
) -> Claimed {
    let mut claimed = Claimed::default();
    let mut finishing = None;
    for index in tasks {
        let began = Instant::now();
        let claim = Op::Claim {
            task: TaskId(format!("t{index}")),
            lease_ms: LEASE_MS,
            fence: None,
            cap: None,
        };
        let applied = match committer
            .submit(write(&by, format!("claim:{index}"), claim))
            .await
        {
            Ok(applied) => applied,
            Err(error) => {
                note(&error);
                claimed.refused += 1;
                continue;
            }
        };
        claimed.latencies.push(micros(began));
        if let Some(previous) = finishing.take() {
            claimed.settle(previous, started).await;
        }
        let finish = finish(
            committer.clone(),
            by.clone(),
            index,
            ClaimId(applied.event.0),
        );
        if pipeline {
            finishing = Some(nanocode_executor::spawn(finish));
        } else {
            claimed
                .settle(nanocode_executor::spawn(finish), started)
                .await;
        }
    }
    if let Some(previous) = finishing {
        claimed.settle(previous, started).await;
    }
    claimed
}

impl Claimed {
    async fn settle(&mut self, finish: nanocode_executor::JoinHandle<bool>, started: Instant) {
        if !finish.await.unwrap_or(false) {
            self.refused += 1;
        }
        self.done_at.push(micros(started));
    }
}

fn fail(what: &str) -> std::process::ExitCode {
    eprintln!("claims-pg: {what}");
    std::process::ExitCode::FAILURE
}

#[allow(clippy::too_many_lines)]
fn main() -> std::process::ExitCode {
    let options = match Options::parse() {
        Ok(options) => options,
        Err(error) => return fail(&error),
    };
    let (Some(count), Some(log_count)) = (
        NonZeroU32::new(options.parts),
        NonZeroU32::new(options.log_parts),
    ) else {
        return fail("parts and log_parts must be at least 1");
    };
    let Ok(runtime) = nanocode_executor::Builder::new_multi_thread()
        .worker_threads(options.threads)
        .build()
    else {
        return fail("no runtime");
    };
    let pg = match PgOptions::parse(&options.url) {
        Ok(pg) => pg,
        Err(error) => return fail(&format!("bad url: {error}")),
    };
    let pool = PgPool::new(pg, options.conns);
    let sql = Arc::new(Counted {
        inner: PgSql::reading(pool.clone(), pool),
        trips: AtomicU64::new(0),
    });
    let store = Arc::new(SqlParts::new(Arc::clone(&sql)));
    runtime.spawn(store.notifier());
    let log = Arc::new(
        PartedLog::fenced(Arc::clone(&store), log_count, "bench").with_max_rows(options.max_rows),
    );
    let setup = runtime.block_on(async {
        log.prepare().await?;
        for part in 0..options.log_parts {
            log.claim(Partition(part), 0, u64::MAX / 4, false).await?;
        }
        Ok::<(), StoreError>(())
    });
    if let Err(error) = setup {
        return fail(&format!("log setup: {error}"));
    }
    let host: Arc<dyn LogHost> = Arc::clone(&log) as Arc<dyn LogHost>;
    let identity = match Identity::verify(&Tokens, "token:agent:node0") {
        Ok(identity) => identity,
        Err(error) => return fail(&format!("identity: {error:?}")),
    };
    let boards = match runtime.block_on(shards::open(&host, SCOPE, count, &identity)) {
        Ok(boards) => boards,
        Err(error) => return fail(&format!("open board: {error:?}")),
    };
    let mut committers = Vec::new();
    let mut drives = Vec::new();
    for board in boards {
        let (committer, drive) = Committer::start(board, options.batch);
        committers.push(committer);
        drives.push(runtime.spawn(async move { drive.await.1 }));
    }

    // Every task's home partition, as the board routes a root task without labels.
    let mut homes: Vec<Vec<usize>> = vec![Vec::new(); options.parts as usize];
    for index in 0..options.tasks {
        homes[partition_of(format!("t{index}").as_bytes(), count).0 as usize].push(index);
    }
    // Post every task, many posts in flight so they group-commit.
    let posted = runtime.block_on(async {
        charter(&committers).await?;
        let posters = 256.min(options.tasks.max(1));
        let handles: Vec<_> = (0..posters)
            .map(|poster| {
                let committers = committers.clone();
                let tasks = options.tasks;
                runtime.spawn(async move {
                    for index in (poster..tasks).step_by(posters) {
                        let home = partition_of(format!("t{index}").as_bytes(), count);
                        let post = Op::Post { task: task(index) };
                        committers[home.0 as usize]
                            .submit(write(OWNER, format!("post:t{index}"), post))
                            .await?;
                    }
                    Ok::<(), BoardError>(())
                })
            })
            .collect();
        for handle in handles {
            handle
                .await
                .map_err(|error| BoardError::Dropped(format!("poster: {error}")))??;
        }
        Ok::<(), BoardError>(())
    });
    if let Err(error) = posted {
        return fail(&format!("posting: {error:?}"));
    }

    // Claimer i works partition i % parts, splitting its tasks with that partition's other
    // claimers: claim a task, finish it, take the next.
    let trips_before = sql.trips.load(Ordering::Relaxed);
    let log_before = log.stats();
    let started = Instant::now();
    let parts = options.parts as usize;
    let handles: Vec<_> = (0..options.claimers)
        .map(|index| {
            let part = index % parts;
            let peers = options.claimers / parts + usize::from(part < options.claimers % parts);
            let mine: Vec<usize> = homes[part]
                .iter()
                .copied()
                .skip(index / parts)
                .step_by(peers.max(1))
                .collect();
            runtime.spawn(claimer(
                committers[part].clone(),
                format!("agent:w{index}"),
                mine,
                started,
                options.pipeline,
            ))
        })
        .collect();
    let mut all = Claimed::default();
    runtime.block_on(async {
        for handle in handles {
            if let Ok(claimed) = handle.await {
                all.latencies.extend(claimed.latencies);
                all.done_at.extend(claimed.done_at);
                all.refused += claimed.refused;
            }
        }
    });
    let elapsed = started.elapsed().as_secs_f64();
    let trips = sql.trips.load(Ordering::Relaxed) - trips_before;
    let stats = log.stats();
    drop(committers);
    let (mut rounds, mut writes) = (0, 0);
    runtime.block_on(async {
        for drive in drives {
            if let Ok(stats) = drive.await {
                rounds += stats.appends;
                writes += stats.writes;
            }
        }
    });
    let Claimed {
        mut latencies,
        mut done_at,
        refused,
    } = all;
    latencies.sort_unstable();
    done_at.sort_unstable();
    let claims = latencies.len() as f64;
    let tenth = done_at.len() * 9 / 10;
    let steady = done_at
        .get(tenth)
        .map_or(0.0, |at| tenth as f64 / (*at as f64 / 1e6));
    let commits = stats.commits - log_before.commits;
    let appends = stats.appends - log_before.appends;
    println!(
        "METRIC parts={} log_parts={} claimers={} tasks={} batch={} pipeline={} claims_per_s={:.0} steady_per_s={steady:.0} p50_ms={:.2} p99_ms={:.2} max_ms={:.2} pg_txns_per_claim={:.2} writes_per_append={:.2} appends_per_commit={:.2} refused={refused} seconds={elapsed:.2} first_error={:?}",
        options.parts,
        options.log_parts,
        options.claimers,
        options.tasks,
        options.batch,
        u8::from(options.pipeline),
        claims / elapsed,
        percentile(&latencies, 0.5),
        percentile(&latencies, 0.99),
        percentile(&latencies, 1.0),
        trips as f64 / claims.max(1.0),
        writes as f64 / (rounds as f64).max(1.0),
        appends as f64 / (commits as f64).max(1.0),
        FIRST_ERROR
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take(),
    );
    if refused > 0 || latencies.len() != options.tasks {
        return std::process::ExitCode::FAILURE;
    }
    std::process::ExitCode::SUCCESS
}
