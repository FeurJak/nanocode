use std::collections::BTreeMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll};

use nanocode_durability_core::{StoreError, StoreFuture};
use nanocode_subagents::{AgentId, BoardCoordination, BoardStats, Coordination, Dispatching};
use nanocode_turnlog::{LogHost, MemoryLog};

const HOSTED_IDLE_RETIRE_MS: u64 = 24 * 60 * 60 * 1_000;

static ENABLED: AtomicBool = AtomicBool::new(false);
static DELAYED: AtomicBool = AtomicBool::new(false);
static MUTATION: Mutex<HostMutation> = Mutex::new(HostMutation::None);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HostMutation {
    None,
    PolledOnce,
    AppendUnflushed,
    ReopenLost,
    SteersTakenMidTurn,
    SteersNeverDelivered,
}

impl HostMutation {
    const ALL: [Self; 6] = [
        Self::None,
        Self::PolledOnce,
        Self::AppendUnflushed,
        Self::ReopenLost,
        Self::SteersTakenMidTurn,
        Self::SteersNeverDelivered,
    ];

    const fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::PolledOnce => "polled_once",
            Self::AppendUnflushed => "append_unflushed",
            Self::ReopenLost => "reopen_lost",
            Self::SteersTakenMidTurn => "steers_taken_mid_turn",
            Self::SteersNeverDelivered => "steers_never_delivered",
        }
    }
}
thread_local! {
    static SEED: std::cell::RefCell<Option<Seed>> = const { std::cell::RefCell::new(None) };
}
static TOTALS: Mutex<Option<(u64, BoardStats)>> = Mutex::new(None);

struct Seed {
    host: Arc<dyn LogHost>,
    planes: Vec<Arc<BoardCoordination>>,
    terminal: BTreeMap<String, String>,
    scopes: Vec<String>,
    audits: u64,
    sent: u64,
    stats: BoardStats,
}

impl Seed {
    fn new(seed: u64) -> Self {
        let log = MemoryLog::new();
        let host: Arc<dyn LogHost> = if DELAYED.load(Ordering::Relaxed) {
            Arc::new(Delayed {
                log,
                state: AtomicU64::new(seed | 1),
                appends: AtomicU64::new(0),
                mutation: *lock(&MUTATION),
            })
        } else {
            Arc::new(log)
        };
        Self {
            host,
            planes: Vec::new(),
            terminal: BTreeMap::new(),
            scopes: Vec::new(),
            audits: 0,
            sent: 0,
            stats: BoardStats::default(),
        }
    }
}

struct Yield(bool);

impl Future for Yield {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<()> {
        if self.0 {
            Poll::Ready(())
        } else {
            self.0 = true;
            context.waker().wake_by_ref();
            Poll::Pending
        }
    }
}

struct Delayed {
    log: MemoryLog,
    state: AtomicU64,
    appends: AtomicU64,
    mutation: HostMutation,
}

impl Delayed {
    fn yields(&self) -> u64 {
        let mut x = self.state.load(Ordering::Relaxed);
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state.store(x, Ordering::Relaxed);
        1 + x % 3
    }

    fn delayed<'a, T: Send + 'a>(
        &'a self,
        operation: StoreFuture<'a, Result<T, StoreError>>,
    ) -> StoreFuture<'a, Result<T, StoreError>> {
        let before = self.yields();
        let after = self.yields();
        Box::pin(async move {
            for _ in 0..before {
                Yield(false).await;
            }
            let output = operation.await;
            for _ in 0..after {
                Yield(false).await;
            }
            output
        })
    }
}

impl LogHost for Delayed {
    fn scan<'a>(
        &'a self,
        scope: &'a str,
        from: u64,
        to: u64,
    ) -> StoreFuture<'a, Result<Vec<(u64, String)>, StoreError>> {
        self.delayed(self.log.scan(scope, from, to))
    }

    fn load<'a>(
        &'a self,
        scope: &'a str,
    ) -> StoreFuture<'a, Result<Vec<(u64, String)>, StoreError>> {
        self.delayed(self.log.load(scope))
    }

    fn append<'a>(
        &'a self,
        scope: &'a str,
        rows: &'a [(u64, String)],
    ) -> StoreFuture<'a, Result<(), StoreError>> {
        let appended = self.appends.fetch_add(1, Ordering::Relaxed);
        if self.mutation == HostMutation::AppendUnflushed && appended % 7 == 6 {
            return self.delayed(Box::pin(std::future::ready(Ok(()))));
        }
        self.delayed(self.log.append(scope, rows))
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

pub fn enable() {
    ENABLED.store(true, Ordering::Relaxed);
}

pub fn enable_delayed() {
    enable();
    DELAYED.store(true, Ordering::Relaxed);
}

pub fn mutate(name: &str) -> bool {
    let Some(mutation) = HostMutation::ALL
        .into_iter()
        .find(|mutation| mutation.name() == name)
    else {
        eprintln!(
            "--subagents-board-mutation takes {}",
            HostMutation::ALL.map(HostMutation::name).join("|")
        );
        return false;
    };
    enable_delayed();
    *lock(&MUTATION) = mutation;
    true
}

pub fn mutation() -> Option<String> {
    let mutation = *lock(&MUTATION);
    (mutation != HostMutation::None).then(|| format!("subagents_board_{}", mutation.name()))
}

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

pub fn begin(seed: u64) {
    if enabled() {
        let board = match *lock(&MUTATION) {
            HostMutation::PolledOnce => Some(nanocode_board::sim::Mutation::BoardPolledOnce),
            HostMutation::SteersTakenMidTurn => {
                Some(nanocode_board::sim::Mutation::SteersTakenMidTurn)
            }
            HostMutation::SteersNeverDelivered => {
                Some(nanocode_board::sim::Mutation::SteersNeverDelivered)
            }
            _ => None,
        };
        if let Some(board) = board {
            nanocode_board::sim::mutate(board);
        }
        SEED.set(Some(Seed::new(seed)));
    }
}

pub fn plane() -> Option<Arc<dyn Coordination>> {
    if !enabled() {
        return None;
    }
    let reopen_lost = *lock(&MUTATION) == HostMutation::ReopenLost;
    SEED.with_borrow_mut(|seed| {
        let seed = seed.get_or_insert_with(|| Seed::new(0));
        let scope = if seed.planes.is_empty() || !reopen_lost {
            "subagents".to_owned()
        } else {
            format!("subagents-{}", seed.planes.len())
        };
        let plane = BoardCoordination::new(Arc::clone(&seed.host), &scope);
        plane.dispatching(Some(dispatching()));
        seed.scopes.push(scope);
        seed.planes.push(Arc::clone(&plane));
        Some(plane as Arc<dyn Coordination>)
    })
}

/// Production dispatching, except that pooled agents stay placed for the
/// whole run so the judge can read every one of them.
fn dispatching() -> Dispatching {
    let mut dispatching = Dispatching::default();
    dispatching.policy.idle_retire_ms = HOSTED_IDLE_RETIRE_MS;
    dispatching
}

pub fn hosting_plane() -> Option<Arc<BoardCoordination>> {
    SEED.with_borrow(|seed| seed.as_ref()?.planes.last().cloned())
}

pub fn stop_hosting() {
    SEED.with_borrow(|seed| {
        for plane in seed.iter().flat_map(|seed| &seed.planes) {
            plane.dispatching(None);
        }
    });
}

pub async fn steer(root: &str, id: AgentId) -> bool {
    let Some(plane) = SEED.with_borrow(|seed| seed.as_ref()?.planes.last().cloned()) else {
        return false;
    };
    let sent = plane.steer(root, id, format!("steer {id}")).await.is_ok();
    if sent {
        SEED.with_borrow_mut(|seed| {
            if let Some(seed) = seed.as_mut() {
                seed.sent += 1;
            }
        });
    }
    sent
}

pub async fn audit() -> Option<String> {
    let (plane, host, scopes) = SEED.with_borrow(|seed| {
        let seed = seed.as_ref()?;
        Some((
            seed.planes.last().cloned()?,
            Arc::clone(&seed.host),
            seed.scopes.clone(),
        ))
    })?;
    for pair in scopes.windows(2) {
        let before = match host.load(&pair[0]).await {
            Ok(rows) => rows,
            Err(error) => return Some(format!("board: load {}: {error}", pair[0])),
        };
        let after = match host.load(&pair[1]).await {
            Ok(rows) => rows,
            Err(error) => return Some(format!("board: load {}: {error}", pair[1])),
        };
        if !after.starts_with(&before) {
            return Some(format!(
                "board: the restarted coordinator reads {} rows from {}, which lack the {} rows written to {} before the restart",
                after.len(),
                pair[1],
                before.len(),
                pair[0]
            ));
        }
    }
    let stats = match plane.audit().await {
        Ok(stats) => stats,
        Err(error) => return Some(format!("board: {error}")),
    };
    let (entries, _) = plane.terminal_after(0).await;
    let mut terminal = BTreeMap::new();
    for (task, phase) in entries {
        if terminal
            .insert(task.to_string(), format!("{phase:?}"))
            .is_some()
        {
            return Some(format!("board: task {task} finished more than once"));
        }
    }
    SEED.with_borrow_mut(|seed| {
        let seed = seed.as_mut()?;
        for (task, phase) in &seed.terminal {
            match terminal.get(task) {
                Some(now) if now == phase => {}
                now => {
                    return Some(format!(
                        "board: task {task} was {phase} before the restart and is {now:?} after it"
                    ));
                }
            }
        }
        if seed.planes.len() == 1 && stats.delivered + stats.unheard > seed.sent {
            return Some(format!(
                "board: {} steers were sent to running subagents, but {} reached a step boundary and {} were heeded at finish",
                seed.sent, stats.delivered, stats.unheard
            ));
        }
        if stats.missed > 0 {
            return Some(format!(
                "board: {} steers were pending at a later step boundary of their subagent and first heard at its finish",
                stats.missed
            ));
        }
        seed.terminal = terminal;
        seed.audits += 1;
        seed.stats = stats;
        None
    })
}

pub fn finish() -> Option<String> {
    if !enabled() {
        return None;
    }
    let seed = SEED.take()?;
    if !seed.planes.is_empty() && seed.audits == 0 {
        return Some("board: the run ended without auditing its board".to_owned());
    }
    let mut totals = lock(&TOTALS);
    let totals = totals.get_or_insert_with(|| (0, BoardStats::default()));
    totals.0 += 1;
    totals.1.add(&seed.stats);
    None
}

pub fn report() {
    if !enabled() {
        return;
    }
    if let Some((seeds, stats)) = lock(&TOTALS).as_ref() {
        println!("BOARD seeds={seeds} {stats:?}");
    } else {
        println!("BOARD seeds=0");
    }
}
