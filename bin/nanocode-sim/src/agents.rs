use std::{cell::RefCell, sync::Arc, time::Duration};

use nanocode_agent_core::AgentHandle;
use nanocode_board::{TaskId, brief::Brief};
use nanocode_dispatch::{Domain, Spawn};
use nanocode_dispatcher::{Pool, Start};
use nanocode_subagents::{
    AgentId, AgentStatus, AgentTask, Coordination, DEFAULT_MAX_SUBAGENTS, Registry,
    ScopedAgentUpdate, SubagentControl, SubagentPool,
};
use nanocode_sync::mpsc::UnboundedReceiver;
use serde_json::Value;

use crate::{
    executor::{Clock, Ended, Executor, Task},
    rng::Rng,
    scripted::{Counts, Ledger, Script, ScriptedFactory, ScriptedStore, simulated_model},
};

pub const CHILDREN_MIN: u64 = 1;
pub const CHILDREN_MAX: u64 = 64;
pub const RESIDENT_MAX: u64 = 4;
pub const TURN_STEPS_MAX: u64 = 8;
pub const STEP_LATENCY_MS_MIN: u64 = 50;
pub const STEP_LATENCY_MS_MAX: u64 = 20_000;
pub const RESTORE_LATENCY_MS_MAX: u64 = 2_000;
pub const SHUTDOWN_LATENCY_MS_MAX: u64 = 1_000;
pub const CANCEL_PPM: u64 = 250_000;
pub const POOLED_MAX: u64 = 4;
pub const RUNTIME_STOP_TIMEOUT: Duration = Duration::from_secs(30);
const ROOT_SESSION: &str = "root";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AgentsMutation {
    None,
    CancelIgnored,
    ShutdownHangs,
    ShutdownKeepsHarness,
    TurnLeaks,
    BoardFailuresUnread,
    PoolRetireKeepsAgent,
}

impl AgentsMutation {
    const ALL: [Self; 7] = [
        Self::None,
        Self::CancelIgnored,
        Self::ShutdownHangs,
        Self::ShutdownKeepsHarness,
        Self::TurnLeaks,
        Self::BoardFailuresUnread,
        Self::PoolRetireKeepsAgent,
    ];

    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|mutation| mutation.name() == name)
    }

    pub const fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::CancelIgnored => "cancel_ignored",
            Self::ShutdownHangs => "shutdown_hangs",
            Self::ShutdownKeepsHarness => "shutdown_keeps_harness",
            Self::TurnLeaks => "turn_leaks",
            Self::BoardFailuresUnread => "board_failures_unread",
            Self::PoolRetireKeepsAgent => "pool_retire_keeps_agent",
        }
    }

    pub fn names() -> String {
        Self::ALL.map(Self::name).join("|")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stop {
    Close,
    Cancel,
}

struct Shape {
    children: u64,
    pooled: u64,
    resident_max: usize,
    stop_after: Duration,
    stop: Stop,
    script: Script,
}

impl Shape {
    fn of(seed: u64, mutation: AgentsMutation) -> Self {
        let mut rng = Rng::derive(seed, "agents-shape");
        let children = rng.range(CHILDREN_MIN, CHILDREN_MAX);
        let resident_max = rng.range(0, RESIDENT_MAX);
        let stop = if rng.chance(CANCEL_PPM) {
            Stop::Cancel
        } else {
            Stop::Close
        };
        let script = Script {
            steps_max: rng.range(1, TURN_STEPS_MAX),
            step_latency_ms_min: STEP_LATENCY_MS_MIN,
            step_latency_ms_max: rng.range(STEP_LATENCY_MS_MIN, STEP_LATENCY_MS_MAX),
            restore_latency_ms_max: rng.range(0, RESTORE_LATENCY_MS_MAX),
            shutdown_latency_ms_max: rng.range(0, SHUTDOWN_LATENCY_MS_MAX),
            mutation,
        };
        let turn_ms_max = script.steps_max * script.step_latency_ms_max;
        let stop_after = Duration::from_millis(rng.range(0, turn_ms_max));
        let pooled = rng.range(1, POOLED_MAX);
        Self {
            children,
            pooled,
            resident_max: usize::try_from(resident_max).expect("a resident bound fits in usize"),
            stop_after,
            stop,
            script,
        }
    }

    fn describe(&self) -> String {
        format!(
            "scenario=agents children={} pooled={} resident<={} stop={:?} after_ms={} steps<={} latency_ms<={} restore_ms<={} shutdown_ms<={}",
            self.children,
            self.pooled,
            self.resident_max,
            self.stop,
            self.stop_after.as_millis(),
            self.script.steps_max,
            self.script.step_latency_ms_max,
            self.script.restore_latency_ms_max,
            self.script.shutdown_latency_ms_max
        )
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct AgentStats {
    pub seeds: u64,
    pub closes: u64,
    pub cancels: u64,
    pub children: u64,
    pub spawned: u64,
    pub restored: u64,
    pub parked: u64,
    pub steps: u64,
    pub turns: u64,
    pub stop_ms_max: u64,
}

impl AgentStats {
    pub fn add(&mut self, other: &Self) {
        self.seeds += other.seeds;
        self.closes += other.closes;
        self.cancels += other.cancels;
        self.children += other.children;
        self.spawned += other.spawned;
        self.restored += other.restored;
        self.parked += other.parked;
        self.steps += other.steps;
        self.turns += other.turns;
        self.stop_ms_max = self.stop_ms_max.max(other.stop_ms_max);
    }

    fn of(shape: &Shape, counts: Counts, stop_ms: u64) -> Self {
        Self {
            seeds: 1,
            closes: u64::from(shape.stop == Stop::Close),
            cancels: u64::from(shape.stop == Stop::Cancel),
            children: shape.children,
            spawned: counts.spawned,
            restored: counts.restored,
            parked: counts.parked,
            steps: counts.steps,
            turns: counts.turns,
            stop_ms_max: stop_ms,
        }
    }
}

pub struct AgentOutcome {
    pub digest: u64,
    pub wrong: Option<String>,
    pub stats: AgentStats,
    pub shape: String,
}

struct Run {
    registry: Arc<Registry>,
    control: SubagentControl,
    root: AgentHandle,
    updates: UnboundedReceiver<ScopedAgentUpdate>,
    plane: Option<Arc<dyn Coordination>>,
}

fn build(ledger: &Arc<Ledger>, shape: &Shape) -> Run {
    let (registry, control, updates) = nanocode_subagents::channel(DEFAULT_MAX_SUBAGENTS);
    let plane = crate::boardagents::plane();
    control.set_coordination(plane.clone());
    control.set_hibernation(true);
    control.set_max_resident(shape.resident_max);
    registry.set_store(Arc::new(ScriptedStore::new(Arc::clone(ledger))));
    let root = AgentHandle::new(
        ROOT_SESSION,
        simulated_model(),
        Arc::new(ScriptedFactory::new(Arc::clone(ledger))),
    );
    Run {
        registry,
        control,
        root,
        updates,
        plane,
    }
}

fn tasks(children: u64) -> Vec<AgentTask> {
    (0..children)
        .map(|child| AgentTask {
            role: "worker".to_owned(),
            task: format!("task {child}"),
            output_schema: Value::Object(serde_json::Map::new()),
        })
        .collect()
}

async fn stop(run: &Run, shape: &Shape, clock: &Clock) -> Result<u64, String> {
    let began_ns = clock.now_ns();
    match shape.stop {
        Stop::Close => run
            .control
            .close_all(ROOT_SESSION)
            .await
            .map_err(|error| format!("close_all failed: {error}"))?,
        Stop::Cancel => run.control.cancel_all(ROOT_SESSION).await,
    }
    let took = Duration::from_nanos(clock.now_ns() - began_ns);
    let took_ms = u64::try_from(took.as_millis()).expect("a virtual stop fits in u64 milliseconds");
    if took >= RUNTIME_STOP_TIMEOUT {
        return Err(format!(
            "{:?} took {took_ms} ms of virtual time, not under the runtime's stop deadline of {} ms",
            shape.stop,
            RUNTIME_STOP_TIMEOUT.as_millis()
        ));
    }
    Ok(took_ms)
}

const fn settled(stop: Stop, status: &AgentStatus) -> bool {
    match stop {
        Stop::Close => matches!(status, AgentStatus::Closed),
        Stop::Cancel => matches!(
            status,
            AgentStatus::Interrupted | AgentStatus::Completed { .. } | AgentStatus::Failed { .. }
        ),
    }
}

async fn judge(
    run: &Run,
    shape: &Shape,
    ledger: &Ledger,
    retired: &[AgentId],
) -> Result<(), String> {
    let turns = ledger.running_turns();
    if turns > 0 {
        return Err(format!(
            "{turns} children still hold a turn after {:?}",
            shape.stop
        ));
    }
    let open = ledger.open_agents();
    if shape.stop == Stop::Close && open > 0 {
        return Err(format!(
            "{open} children still hold a resident harness after Close"
        ));
    }
    let listed = run
        .registry
        .directory(ROOT_SESSION, true, false)
        .await
        .map_err(|error| {
            format!(
                "the directory did not answer after {:?}: {error}",
                shape.stop
            )
        })?;
    let count = u64::try_from(listed.len()).expect("a child count fits in u64");
    if count != shape.children + shape.pooled {
        return Err(format!(
            "the directory lists {count} of {} children and {} pooled agents",
            shape.children, shape.pooled
        ));
    }
    for entry in listed {
        let closed = retired.contains(&entry.agent_id);
        if closed && entry.status != AgentStatus::Closed {
            return Err(format!(
                "the pool retired agent {} but it is {:?} after {:?}",
                entry.agent_id, entry.status, shape.stop
            ));
        }
        if !closed && !settled(shape.stop, &entry.status) {
            return Err(format!(
                "agent {} is {:?} after {:?}",
                entry.agent_id, entry.status, shape.stop
            ));
        }
    }
    if let Some(plane) = &run.plane {
        board_agrees(run, plane, true).await?;
    }
    Ok(())
}

async fn board_agrees(
    run: &Run,
    plane: &Arc<dyn Coordination>,
    settled: bool,
) -> Result<(), String> {
    let listed = run
        .registry
        .directory(ROOT_SESSION, true, false)
        .await
        .map_err(|error| format!("list_agents from the board failed: {error}"))?;
    let ids = listed
        .iter()
        .map(|entry| entry.agent_id)
        .collect::<Vec<_>>();
    let (waited, ..) = run
        .registry
        .wait(ROOT_SESSION, "board-agrees", &ids, Duration::ZERO)
        .await
        .map_err(|error| format!("wait_agent from the board failed: {error}"))?;
    run.registry.set_coordination(None);
    let registry = run.registry.directory(ROOT_SESSION, true, false).await;
    run.registry.set_coordination(Some(Arc::clone(plane)));
    let registry = registry.map_err(|error| format!("the registry did not list: {error}"))?;
    let board = plane
        .agents(ROOT_SESSION)
        .await
        .map_err(|error| format!("the board did not read: {error}"))?;
    let truth = |id| {
        registry
            .iter()
            .find(|entry| entry.agent_id == id)
            .map(|entry| &entry.status)
    };
    let finished = |status: &AgentStatus| {
        matches!(
            status,
            AgentStatus::Completed { .. } | AgentStatus::Failed { .. }
        )
    };
    if settled && listed.len() != registry.len() {
        return Err(format!(
            "list_agents from the board lists {} of the registry's {} children",
            listed.len(),
            registry.len()
        ));
    }
    let answers = listed
        .iter()
        .map(|entry| ("list_agents", entry.agent_id, &entry.status))
        .chain(
            waited
                .iter()
                .map(|summary| ("wait_agent", summary.agent_id, &summary.status)),
        );
    for entry in &registry {
        let read = board.get(&entry.agent_id);
        let agrees = match read {
            _ if !finished(&entry.status) => true,
            Some(Some(status)) => *status == entry.status,
            _ => !settled,
        };
        if !agrees {
            return Err(format!(
                "the board reads agent {} as {read:?}, the registry {:?}",
                entry.agent_id, entry.status
            ));
        }
    }
    for (tool, id, status) in answers {
        let expected = truth(id);
        let lagging = !settled && !finished(status);
        if expected != Some(status) && !lagging {
            return Err(format!(
                "{tool} from the board says agent {id} is {status:?}, the registry {expected:?}"
            ));
        }
    }
    Ok(())
}

fn start_of(index: u64) -> Start {
    let intent = TaskId(format!("intent/pooled-{index}"));
    let task = TaskId(format!("pooled-{index}"));
    Start {
        spawn: Spawn {
            intent,
            task: task.clone(),
            domain: Domain {
                role: "worker".to_owned(),
                member: "member".to_owned(),
                project: "project".to_owned(),
            },
            placement: "sim".to_owned(),
            cost_nano_usd: 1,
        },
        agent: format!("pooled-{index}"),
        worker: format!("dispatcher+pooled-{index}"),
        brief: Brief {
            task,
            text: format!("pooled task {index}"),
            tokens: 4,
            steers: Vec::new(),
            omitted_bytes: 0,
        },
    }
}

async fn pooled(run: &Run, shape: &Shape, clock: &Arc<Clock>) -> Result<Vec<AgentId>, String> {
    let now = Arc::clone(clock);
    let pool = SubagentPool::new(
        run.root.clone(),
        Arc::clone(&run.registry),
        ROOT_SESSION,
        Box::new(move || now.now_ns() / 1_000_000),
    );
    for index in 0..shape.pooled {
        pool.start(start_of(index))
            .await
            .map_err(|error| format!("the pool did not start pooled-{index}: {error:?}"))?;
    }
    pool.start(start_of(0))
        .await
        .map_err(|error| format!("the pool did not restart pooled-0: {error:?}"))?;
    let listed = pool.agents().await;
    for index in 0..shape.pooled {
        let name = format!("pooled-{index}");
        let Some(agent) = listed.iter().find(|agent| agent.id == name) else {
            return Err(format!("the pool started {name} but does not list it"));
        };
        if agent.intent != Some(start_of(index).spawn.intent) {
            return Err(format!("the pool lists {name} under {:?}", agent.intent));
        }
    }
    let Some(gone) = pool.agent_id("pooled-0") else {
        return Err("the pool forgot pooled-0 before retiring it".to_owned());
    };
    pool.retire("pooled-0")
        .await
        .map_err(|error| format!("the pool did not retire pooled-0: {error:?}"))?;
    let directory = run
        .registry
        .directory(ROOT_SESSION, true, false)
        .await
        .map_err(|error| format!("the directory did not answer after a retire: {error}"))?;
    let status = directory
        .iter()
        .find(|entry| entry.agent_id == gone)
        .map(|entry| &entry.status);
    if status != Some(&AgentStatus::Closed) {
        return Err(format!(
            "the pool retired pooled-0 (agent {gone}) but the runtime has it {status:?}"
        ));
    }
    if pool.agents().await.iter().any(|agent| agent.id == "pooled-0") {
        return Err("the pool still lists pooled-0 after retiring it".to_owned());
    }
    Ok(vec![gone])
}

async fn drive(shape: &Shape, ledger: &Arc<Ledger>, clock: &Arc<Clock>) -> Result<u64, String> {
    let run = build(ledger, shape);
    nanocode_subagents::start_agents(
        &run.root,
        &run.registry,
        ROOT_SESSION,
        tasks(shape.children),
    )
    .await
    .map_err(|error| format!("start_agents failed: {error}"))?;
    ledger.trace("started");
    let retired = pooled(&run, shape, clock).await?;
    ledger.trace("pooled");
    nanocode_platform::sleep(shape.stop_after).await;
    if let Some(plane) = &run.plane {
        board_agrees(&run, plane, false).await?;
    }
    ledger.trace("stop");
    let took_ms = stop(&run, shape, clock).await?;
    ledger.trace("stopped");
    nanocode_platform::sleep(RUNTIME_STOP_TIMEOUT).await;
    judge(&run, shape, ledger, &retired).await?;
    drop(run.updates);
    Ok(took_ms)
}

pub fn simulate(seed: u64, mutation: AgentsMutation) -> AgentOutcome {
    if mutation == AgentsMutation::BoardFailuresUnread {
        nanocode_board::sim::mutate(nanocode_board::sim::Mutation::SubagentFailuresUnread);
    }
    if mutation == AgentsMutation::PoolRetireKeepsAgent {
        nanocode_board::sim::mutate(nanocode_board::sim::Mutation::PoolRetireKeepsAgent);
    }
    let shape = Shape::of(seed, mutation);
    let clock = Clock::new();
    crate::host::enter(seed, &clock);
    let ledger = Ledger::new(seed, shape.script, Arc::clone(&clock));
    let result = RefCell::new(None);
    let mut executor = Executor::new(Arc::clone(&clock), Rng::derive(seed, "scheduler"));
    let task: Task<'_> = Box::pin(async {
        let outcome = drive(&shape, &ledger, &clock).await;
        result.replace(Some(outcome));
    });
    let ended = executor.run(vec![task], None);
    let outcome = match (ended, result.into_inner()) {
        (_, Some(outcome)) => outcome,
        (Ended::Stuck, None) => Err("every task waits and no timer is set".to_owned()),
        (ended, None) => Err(format!("the run ended {ended:?} without a verdict")),
    };
    let stop_ms = *outcome.as_ref().unwrap_or(&0);
    nanocode_board::sim::mutate(nanocode_board::sim::Mutation::None);
    AgentOutcome {
        digest: ledger.digest(),
        wrong: outcome.err(),
        stats: AgentStats::of(&shape, ledger.counts(), stop_ms),
        shape: shape.describe(),
    }
}

pub fn run_seed(seed: u64, mutation: AgentsMutation) -> AgentOutcome {
    std::panic::catch_unwind(|| simulate(seed, mutation)).unwrap_or_else(|panic| {
        let message = panic
            .downcast_ref::<String>()
            .cloned()
            .or_else(|| panic.downcast_ref::<&str>().map(|s| (*s).to_owned()))
            .unwrap_or_default();
        AgentOutcome {
            digest: 0,
            wrong: Some(format!("panic: {message}")),
            stats: AgentStats::default(),
            shape: String::new(),
        }
    })
}
