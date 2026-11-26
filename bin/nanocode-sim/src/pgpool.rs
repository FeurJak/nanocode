use std::{
    cell::RefCell, future::Future, ops::Range, pin::Pin, process::ExitCode, rc::Rc, sync::Arc,
    time::Duration,
};

use nanocode_durability_core::{Settle, SqlAccess, SqlRow, SqlStatement, StoreError};
use nanocode_store_sql::{
    Answering, Lending, Lent, PgBounds, PgError, PgLimits, answer_within,
    sim::{self as pool_sim, Mutation as Rule},
};

use crate::{
    executor::{Clock, Ended, Executor, Task},
    rng::{FNV_OFFSET, Rng, fnv},
};

const CONNECTIONS_MAX: u64 = 4;
const CALLERS_PER_CONNECTION_MAX: u64 = 3;
const ANSWERED_PPM: u64 = 300_000;
const REFUSED_PPM: u64 = 100_000;
const CALLS_MAX: u64 = 8;
const NEVER_PPM: u64 = 500_000;
const ACCESSES: [SqlAccess; 3] = [SqlAccess::Read, SqlAccess::Write, SqlAccess::Schema];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PgPoolMutation {
    None,
    WaitsUnbounded,
    CallBoundSkipsCancel,
}

impl PgPoolMutation {
    const ALL: [Self; 3] = [Self::None, Self::WaitsUnbounded, Self::CallBoundSkipsCancel];

    const fn rule(self) -> Rule {
        match self {
            Self::None => Rule::None,
            Self::WaitsUnbounded => Rule::WaitsUnbounded,
            Self::CallBoundSkipsCancel => Rule::CallBoundSkipsCancel,
        }
    }

    pub const fn name(self) -> &'static str {
        self.rule().name()
    }

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|mutation| mutation.name() == name)
    }

    pub fn names() -> String {
        Self::ALL[1..]
            .iter()
            .map(|mutation| mutation.name())
            .collect::<Vec<_>>()
            .join("|")
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct PgPoolStats {
    pub seeds: u64,
    pub callers: u64,
    pub lent: u64,
    pub busy: u64,
    pub refused: u64,
    pub calls: u64,
    pub unknown: u64,
}

impl PgPoolStats {
    pub const fn add(&mut self, other: &Self) {
        self.seeds += other.seeds;
        self.callers += other.callers;
        self.lent += other.lent;
        self.busy += other.busy;
        self.refused += other.refused;
        self.calls += other.calls;
        self.unknown += other.unknown;
    }
}

struct Connection;

#[derive(Debug)]
struct Refusal;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Store {
    Unanswered,
    Answers,
    Refuses,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Answer {
    Lent,
    Busy,
    Refused,
}

type Connecting = Pin<Box<dyn Future<Output = Result<Connection, Refusal>>>>;

struct Outcome {
    digest: u64,
    wrong: Option<String>,
    stats: PgPoolStats,
    shape: String,
}

fn connecting(store: Store) -> Connecting {
    match store {
        Store::Unanswered => Box::pin(std::future::pending()),
        Store::Answers => Box::pin(std::future::ready(Ok(Connection))),
        Store::Refuses => Box::pin(std::future::ready(Err(Refusal))),
    }
}

fn run_seed(seed: u64, mutation: PgPoolMutation) -> Outcome {
    pool_sim::mutate(mutation.rule());
    let clock = Clock::new();
    let mut outcome = crate::host::entered(seed, &clock, || callers(seed, &clock));
    let clock = Clock::new();
    let calls = crate::host::entered(seed, &clock, || bound_calls(seed, &clock));
    pool_sim::mutate(Rule::None);
    outcome.digest = fnv(outcome.digest, calls.shape.as_bytes());
    outcome.wrong = outcome.wrong.or(calls.wrong);
    outcome.stats.add(&calls.stats);
    outcome.shape.push_str(&calls.shape);
    outcome
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Reply {
    Never,
    Answers,
}

#[derive(Clone, Copy, Debug)]
struct Call {
    delay_ms: u64,
    store: Store,
    connect_ms: u64,
    statement: Reply,
    cancel: Reply,
    access: SqlAccess,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Called {
    Answered,
    Busy,
    Unknown,
    Refused,
}

struct Statement(Call);

impl Answering for Statement {
    type Key = Reply;

    fn key(&self) -> Reply {
        self.0.cancel
    }

    fn answer<'a>(
        &'a mut self,
        _settle: Settle,
        _bounds: PgBounds,
        _statements: &'a [SqlStatement<'a>],
    ) -> impl Future<Output = Result<Vec<Vec<SqlRow>>, PgError>> + Send + 'a {
        let reply = self.0.statement;
        async move {
            if reply == Reply::Never {
                std::future::pending::<()>().await;
            }
            Ok(Vec::new())
        }
    }
}

async fn made(call: Call) -> Result<Statement, Refusal> {
    match call.store {
        Store::Unanswered => std::future::pending().await,
        Store::Refuses => Err(Refusal),
        Store::Answers => {
            nanocode_platform::sleep(Duration::from_millis(call.connect_ms)).await;
            Ok(Statement(call))
        }
    }
}

async fn cancelled(reply: Reply) -> Result<(), PgError> {
    if reply == Reply::Never {
        std::future::pending::<()>().await;
    }
    Ok(())
}

fn reply(rng: &mut Rng) -> Reply {
    if rng.chance(NEVER_PPM) {
        Reply::Never
    } else {
        Reply::Answers
    }
}

fn limits(rng: &mut Rng) -> PgLimits {
    let defaults = PgLimits::default();
    let acquire_ms =
        u64::try_from(defaults.acquire.as_millis()).expect("an acquire deadline fits in u64");
    let statement_ms = u64::try_from(defaults.write.statement.as_millis())
        .expect("a statement timeout fits in u64");
    let bounds = PgBounds {
        statement: Duration::from_millis(rng.range(1, statement_ms + 1)),
        ..defaults.write
    };
    PgLimits {
        acquire: Duration::from_millis(rng.range(1, acquire_ms + 1)),
        read: bounds,
        write: bounds,
        schema: bounds,
        autocommit: bounds,
    }
}

fn bound_calls(seed: u64, clock: &Arc<Clock>) -> Outcome {
    let mut rng = Rng::derive(seed, "pg pool bound calls");
    let limits = limits(&mut rng);
    let acquire_ms =
        u64::try_from(limits.acquire.as_millis()).expect("an acquire deadline fits in u64");
    let bound_ns = u64::try_from(limits.call_bound().as_nanos())
        .expect("a call bound fits in u64 nanoseconds");
    let connections = rng.range(1, CONNECTIONS_MAX + 1);
    let lending: Lending<Statement> =
        Lending::new(usize::try_from(connections).expect("a connection count fits in usize"));
    let plan: Vec<Call> = (0..rng.range(1, CALLS_MAX + 1))
        .map(|_| Call {
            delay_ms: rng.below(acquire_ms),
            store: if rng.chance(ANSWERED_PPM) || rng.chance(ANSWERED_PPM) {
                Store::Answers
            } else if rng.chance(REFUSED_PPM) {
                Store::Refuses
            } else {
                Store::Unanswered
            },
            connect_ms: rng.below(acquire_ms),
            statement: reply(&mut rng),
            cancel: reply(&mut rng),
            access: ACCESSES[usize::try_from(rng.below(3)).expect("an index fits in usize")],
        })
        .collect();
    let ended: Rc<RefCell<Vec<Option<(u64, u64, Called)>>>> =
        Rc::new(RefCell::new(vec![None; plan.len()]));
    let tasks = plan
        .iter()
        .enumerate()
        .map(|(index, &call)| {
            let (lending, limits, clock, ended) =
                (&lending, &limits, Arc::clone(clock), Rc::clone(&ended));
            Box::pin(async move {
                nanocode_platform::sleep(Duration::from_millis(call.delay_ms)).await;
                let started = clock.now_ns();
                let called = match lending.lend(limits.acquire, |_| false, || made(call)).await {
                    Err(_) => Called::Busy,
                    Ok(Err(Refusal)) => Called::Refused,
                    Ok(Ok(lent)) => {
                        match answer_within(
                            lent,
                            limits,
                            call.access,
                            Settle::Durable,
                            &[],
                            cancelled,
                        )
                        .await
                        {
                            Ok(_) => Called::Answered,
                            Err(StoreError::Busy) => Called::Busy,
                            Err(_) => Called::Unknown,
                        }
                    }
                };
                ended.borrow_mut()[index] = Some((started, clock.now_ns(), called));
            }) as Task<'_>
        })
        .collect();
    let run = Executor::new(
        Arc::clone(clock),
        Rng::derive(seed, "pg pool bound callers"),
    )
    .run(tasks, None);
    let mut stats = PgPoolStats::default();
    let mut shape = format!(" calls={} bound_ms={} ", plan.len(), bound_ns / 1_000_000);
    let wrong = called(
        &plan,
        &ended.borrow(),
        run,
        bound_ns,
        &mut stats,
        &mut shape,
    );
    Outcome {
        digest: fnv(FNV_OFFSET, shape.as_bytes()),
        wrong,
        stats,
        shape,
    }
}

fn called(
    plan: &[Call],
    ended: &[Option<(u64, u64, Called)>],
    run: Ended,
    bound_ns: u64,
    stats: &mut PgPoolStats,
    shape: &mut String,
) -> Option<String> {
    if run != Ended::Finished {
        return Some(format!(
            "with a store that never answers, bound calls still wait: {run:?}"
        ));
    }
    for (index, (call, end)) in plan.iter().zip(ended).enumerate() {
        let Some((started, ended, answer)) = *end else {
            return Some(format!("bound call {index} never returned"));
        };
        stats.calls += 1;
        shape.push(match answer {
            Called::Answered => 'a',
            Called::Busy => 'b',
            Called::Unknown => 'u',
            Called::Refused => 'r',
        });
        if answer == Called::Unknown {
            stats.unknown += 1;
        }
        let waited = ended - started;
        if waited > bound_ns {
            return Some(format!(
                "bound call {index} ({call:?}) answered {answer:?} after {waited} ns, past the call bound of {bound_ns} ns its store reports"
            ));
        }
        let made = call.store == Store::Answers;
        let wrong = match answer {
            Called::Answered => !made || call.statement == Reply::Never,
            Called::Unknown => {
                !made || call.statement == Reply::Answers || call.access == SqlAccess::Read
            }
            Called::Refused => call.store != Store::Refuses,
            Called::Busy => false,
        };
        if wrong || (call.store == Store::Unanswered && answer != Called::Busy) {
            return Some(format!("bound call {index} ({call:?}) answered {answer:?}"));
        }
    }
    None
}

fn callers(seed: u64, clock: &Arc<Clock>) -> Outcome {
    let mut rng = Rng::derive(seed, "pg pool");
    let wait = PgLimits::default().acquire;
    let wait_ns = u64::try_from(wait.as_nanos()).expect("the acquire deadline fits in u64");
    let wait_ms = u64::try_from(wait.as_millis()).expect("the acquire deadline fits in u64");
    let connections = rng.range(1, CONNECTIONS_MAX + 1);
    let count = connections + rng.range(1, connections * CALLERS_PER_CONNECTION_MAX + 1);
    let lending: Lending<Connection> =
        Lending::new(usize::try_from(connections).expect("a connection count fits in usize"));
    let plan: Vec<(u64, Store)> = (0..count)
        .map(|_| {
            let store = if rng.chance(ANSWERED_PPM) {
                Store::Answers
            } else if rng.chance(REFUSED_PPM) {
                Store::Refuses
            } else {
                Store::Unanswered
            };
            (rng.below(wait_ms), store)
        })
        .collect();
    let held: Rc<RefCell<Vec<Lent<Connection>>>> = Rc::default();
    let ended: Rc<RefCell<Vec<Option<(u64, u64, Answer)>>>> =
        Rc::new(RefCell::new(vec![None; plan.len()]));
    let tasks = plan
        .iter()
        .enumerate()
        .map(|(index, &(delay_ms, store))| {
            let (lending, clock, held, ended) = (
                &lending,
                Arc::clone(clock),
                Rc::clone(&held),
                Rc::clone(&ended),
            );
            Box::pin(async move {
                nanocode_platform::sleep(Duration::from_millis(delay_ms)).await;
                let started = clock.now_ns();
                let lent = lending.lend(wait, |_| true, || connecting(store)).await;
                let answer = match lent {
                    Ok(Ok(lent)) => {
                        held.borrow_mut().push(lent);
                        Answer::Lent
                    }
                    Ok(Err(Refusal)) => Answer::Refused,
                    Err(_) => Answer::Busy,
                };
                ended.borrow_mut()[index] = Some((started, clock.now_ns(), answer));
            }) as Task<'_>
        })
        .collect();
    let run =
        Executor::new(Arc::clone(clock), Rng::derive(seed, "pg pool callers")).run(tasks, None);
    let mut stats = PgPoolStats {
        seeds: 1,
        callers: count,
        ..PgPoolStats::default()
    };
    let mut shape = format!("connections={connections} callers={count} ");
    let wrong = judged(
        &plan,
        &ended.borrow(),
        run,
        wait_ns,
        connections,
        &mut stats,
        &mut shape,
    );
    let lent = held.borrow().len();
    let wrong = wrong.or_else(|| {
        (u64::try_from(lent).expect("a lent count fits in u64") > connections)
            .then(|| format!("{lent} connections are lent at once from a pool of {connections}"))
    });
    Outcome {
        digest: fnv(FNV_OFFSET, shape.as_bytes()),
        wrong,
        stats,
        shape,
    }
}

fn judged(
    plan: &[(u64, Store)],
    ended: &[Option<(u64, u64, Answer)>],
    run: Ended,
    wait_ns: u64,
    connections: u64,
    stats: &mut PgPoolStats,
    shape: &mut String,
) -> Option<String> {
    if run != Ended::Finished {
        return Some(format!(
            "with a store that never answers, callers of a pool of {connections} still wait: {run:?}"
        ));
    }
    for (index, (&(_, store), end)) in plan.iter().zip(ended).enumerate() {
        let Some((started, ended, answer)) = *end else {
            return Some(format!("caller {index} never returned"));
        };
        shape.push(match answer {
            Answer::Lent => 'l',
            Answer::Busy => 'b',
            Answer::Refused => 'r',
        });
        match answer {
            Answer::Lent => stats.lent += 1,
            Answer::Busy => stats.busy += 1,
            Answer::Refused => stats.refused += 1,
        }
        let waited = ended - started;
        if waited > wait_ns {
            return Some(format!(
                "caller {index} returned {answer:?} after {waited} ns, past its deadline of {wait_ns} ns"
            ));
        }
        if answer == Answer::Lent && store != Store::Answers {
            return Some(format!(
                "caller {index} was lent a connection its store ({store:?}) never made"
            ));
        }
        if answer == Answer::Refused && store != Store::Refuses {
            return Some(format!(
                "caller {index} was refused by a store ({store:?}) that never refused"
            ));
        }
        if store == Store::Unanswered && answer != Answer::Busy {
            return Some(format!(
                "caller {index} answered {answer:?} from a store that never answers"
            ));
        }
    }
    None
}

pub fn judge(
    seed: u64,
    mutation: PgPoolMutation,
    twice: bool,
    verbose: bool,
    totals: &mut PgPoolStats,
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

pub fn replay(seed: u64, mutation: PgPoolMutation) -> String {
    let mut command = format!("nanocode-sim --scenario pg_pool --seed {seed}");
    if mutation != PgPoolMutation::None {
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
    mutation: PgPoolMutation,
    totals: &PgPoolStats,
) -> ExitCode {
    println!(
        "RESULT seeds={}..{} scenario=pg_pool mutation={} passed={passed} failed={failed} twice={twice} stats={totals:?}",
        seeds.start,
        seeds.end,
        mutation.name()
    );
    match mutation {
        PgPoolMutation::None if failed == 0 => ExitCode::SUCCESS,
        PgPoolMutation::None => ExitCode::FAILURE,
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
