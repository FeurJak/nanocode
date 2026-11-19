use std::collections::{BTreeMap, BTreeSet};

use nanocode_durability_core::StoreError;
use serde::{Deserialize, Serialize};

pub mod access;
pub mod mirror;
pub mod sim;
pub mod tasks;

use sim::{Mutation, mutated};
use tasks::{Moved, Refusal, TaskOp, Terms};

const FIRST: u64 = 1;

macro_rules! id {
    ($name:ident) => {
        #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub String);

        impl std::fmt::Display for $name {
            fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str(&self.0)
            }
        }
    };
}

id!(TaskId);
id!(ClaimId);
id!(EventId);

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewTask {
    pub id: TaskId,
    pub parent: Option<TaskId>,
    pub spec: String,
    pub labels: BTreeMap<String, String>,
    pub needs: Vec<TaskId>,
    #[serde(default, skip_serializing_if = "Terms::is_default")]
    pub terms: Terms,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "detail", rename_all = "snake_case")]
pub enum Outcome {
    Done(String),
    Failed(String),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Op {
    Post {
        task: NewTask,
    },
    Claim {
        task: TaskId,
        lease_ms: u64,
    },
    Renew {
        task: TaskId,
        claim: ClaimId,
        lease_ms: u64,
    },
    Release {
        task: TaskId,
        claim: ClaimId,
        note: String,
    },
    Finish {
        task: TaskId,
        claim: ClaimId,
        outcome: Outcome,
    },
    Cancel {
        task: TaskId,
        reason: String,
    },
    Reply {
        task: TaskId,
        body: String,
    },
    Relabel {
        task: TaskId,
        set: BTreeMap<String, String>,
        unset: BTreeSet<String>,
    },
    Move {
        task: TaskId,
        parent: Option<TaskId>,
    },
    Rank {
        task: TaskId,
        rank: Option<u64>,
    },
    Grant {
        grant: access::Grant,
    },
    Revoke {
        grant: EventId,
    },
    Charter {},
    Tasks(TaskOp),
    Sync {
        sync: mirror::SyncOp,
    },
}

impl Op {
    pub const fn task(&self) -> Option<&TaskId> {
        match self {
            Self::Post { task } => Some(&task.id),
            Self::Claim { task, .. }
            | Self::Renew { task, .. }
            | Self::Release { task, .. }
            | Self::Finish { task, .. }
            | Self::Cancel { task, .. }
            | Self::Reply { task, .. }
            | Self::Relabel { task, .. }
            | Self::Rank { task, .. }
            | Self::Move { task, .. } => Some(task),
            Self::Tasks(op) => Some(op.task()),
            Self::Sync { sync } => sync.task(),
            Self::Grant { .. } | Self::Revoke { .. } | Self::Charter {} => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    pub id: EventId,
    pub by: String,
    pub at_ms: u64,
    pub op: Op,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Phase {
    Open,
    Claimed {
        claim: ClaimId,
        worker: String,
        until_ms: u64,
    },
    Done {
        result: String,
    },
    Failed {
        error: String,
    },
    Cancelled {
        reason: String,
    },
    Blocked {
        kind: String,
        reason: String,
    },
}

impl Phase {
    pub const fn terminal(&self) -> bool {
        matches!(
            self,
            Self::Done { .. } | Self::Failed { .. } | Self::Cancelled { .. }
        )
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Task {
    pub spec: NewTask,
    pub phase: Phase,
    pub attempts: u64,
    pub replies: Vec<(String, String)>,
    pub assignee: Option<String>,
    pub rank: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Rejection {
    Exists,
    UnknownTask,
    UnknownParent(TaskId),
    UnknownNeed(TaskId),
    Waiting(TaskId),
    Held { claim: ClaimId, until_ms: u64 },
    NotHolder,
    Terminal,
    Access(access::Denied),
    Tasks(Refusal),
    Sync(mirror::Refusal),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Change {
    Posted,
    Claimed {
        claim: ClaimId,
        worker: String,
        until_ms: u64,
        took_over: Option<ClaimId>,
    },
    Renewed {
        claim: ClaimId,
        until_ms: u64,
    },
    Released {
        claim: ClaimId,
        note: String,
    },
    Finished(Outcome),
    Cancelled {
        reason: String,
    },
    Replied {
        by: String,
        body: String,
    },
    Tasks(Moved),
    Relabeled {
        set: BTreeMap<String, String>,
        unset: BTreeSet<String>,
    },
    Reparented {
        from: Option<TaskId>,
        to: Option<TaskId>,
    },
    Ranked {
        rank: Option<u64>,
    },
    Synced(mirror::Synced),
}

impl Change {
    pub const fn terminal(&self) -> bool {
        matches!(self, Self::Finished(_) | Self::Cancelled { .. })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub seq: u64,
    pub event: EventId,
    pub at_ms: u64,
    pub task: TaskId,
    pub change: Change,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Applied {
    pub seq: u64,
    pub event: EventId,
}

#[derive(Clone, Debug)]
pub enum BoardError {
    Store(StoreError),
    Corrupt { seq: u64, detail: String },
    Rejected(Rejection),
    Unsettled { event: EventId, error: StoreError },
}

impl std::fmt::Display for BoardError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Store(error) => write!(formatter, "board store: {error}"),
            Self::Corrupt { seq, detail } => write!(formatter, "board row {seq}: {detail}"),
            Self::Rejected(rejection) => {
                write!(formatter, "board rejected the write: {rejection:?}")
            }
            Self::Unsettled { event, error } => {
                write!(formatter, "board write {event} is unsettled: {error}")
            }
        }
    }
}

impl From<StoreError> for BoardError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fold {
    next: u64,
    now_ms: u64,
    tasks: BTreeMap<TaskId, Task>,
    children: BTreeMap<TaskId, BTreeSet<TaskId>>,
    dependents: BTreeMap<TaskId, BTreeSet<TaskId>>,
    seen: BTreeMap<EventId, Result<u64, Rejection>>,
    entries: Vec<Entry>,
    access: access::Access,
    mirror: mirror::Mirror,
    live: BTreeSet<TaskId>,
    labelled: BTreeMap<String, BTreeMap<String, BTreeSet<TaskId>>>,
}

impl Default for Fold {
    fn default() -> Self {
        Self::new()
    }
}

impl Fold {
    pub const fn new() -> Self {
        Self {
            next: FIRST,
            now_ms: 0,
            tasks: BTreeMap::new(),
            children: BTreeMap::new(),
            dependents: BTreeMap::new(),
            seen: BTreeMap::new(),
            entries: Vec::new(),
            access: access::Access::new(),
            mirror: mirror::Mirror::new(),
            live: BTreeSet::new(),
            labelled: BTreeMap::new(),
        }
    }

    pub const fn next(&self) -> u64 {
        self.next
    }

    pub const fn now_ms(&self) -> u64 {
        self.now_ms
    }

    pub const fn tasks(&self) -> &BTreeMap<TaskId, Task> {
        &self.tasks
    }

    pub fn task(&self, id: &TaskId) -> Option<&Task> {
        self.tasks.get(id)
    }

    pub fn outcome(&self, event: &EventId) -> Option<&Result<u64, Rejection>> {
        self.seen.get(event)
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    pub fn minted(&self, writer: &str) -> u64 {
        let prefix = format!("{writer}.");
        self.seen
            .range(EventId(prefix.clone())..)
            .map_while(|(id, _)| id.0.strip_prefix(&prefix))
            .filter_map(|count| count.parse::<u64>().ok())
            .max()
            .unwrap_or(0)
    }

    pub fn claimable(&self, id: &TaskId, at_ms: u64) -> Result<Option<ClaimId>, Rejection> {
        let task = self.tasks.get(id).ok_or(Rejection::UnknownTask)?;
        let now = self.now_ms.max(at_ms);
        match &task.phase {
            Phase::Open => {}
            Phase::Claimed {
                claim, until_ms, ..
            } if (*until_ms > now || mutated(Mutation::LeasesNeverExpire))
                && !mutated(Mutation::ClaimIgnoresLease) =>
            {
                return Err(Rejection::Held {
                    claim: claim.clone(),
                    until_ms: *until_ms,
                });
            }
            Phase::Claimed { claim, .. } => return Ok(Some(claim.clone())),
            Phase::Blocked { .. } if !mutated(Mutation::ClaimIgnoresBlock) => {
                return Err(Rejection::Tasks(Refusal::Blocked));
            }
            Phase::Blocked { .. } => {}
            _ => return Err(Rejection::Terminal),
        }
        if mutated(Mutation::ClaimIgnoresNeeds) {
            return Ok(None);
        }
        match task.spec.needs.iter().find(|need| {
            !matches!(
                self.tasks.get(*need).map(|need| &need.phase),
                Some(Phase::Done { .. })
            )
        }) {
            Some(need) => Err(Rejection::Waiting(need.clone())),
            None => Ok(None),
        }
    }

    pub fn ready(&self, at_ms: u64) -> impl Iterator<Item = &TaskId> + '_ {
        self.live
            .iter()
            .filter(move |id| self.claimable(id, at_ms).is_ok())
    }

    pub fn labelled<'a>(&'a self, key: &str, value: &str) -> impl Iterator<Item = &'a TaskId> + 'a {
        self.labelled
            .get(key)
            .and_then(|values| values.get(value))
            .into_iter()
            .flatten()
    }

    pub fn ready_labelled<'a>(
        &'a self,
        key: &str,
        value: &str,
        at_ms: u64,
    ) -> impl Iterator<Item = &'a TaskId> + 'a {
        self.labelled(key, value)
            .filter(move |id| self.claimable(id, at_ms).is_ok())
    }

    fn reindex(&mut self, id: &TaskId) {
        let Some(task) = self.tasks.get(id) else {
            return;
        };
        let live = !task.phase.terminal();
        for (key, value) in &task.spec.labels {
            if live {
                self.labelled
                    .entry(key.clone())
                    .or_default()
                    .entry(value.clone())
                    .or_default()
                    .insert(id.clone());
            } else if let Some(ids) = self
                .labelled
                .get_mut(key)
                .and_then(|values| values.get_mut(value))
            {
                ids.remove(id);
            }
        }
        if live {
            self.live.insert(id.clone());
        } else {
            self.live.remove(id);
        }
    }

    fn unlabel(&mut self, id: &TaskId, keys: impl Iterator<Item = String>) {
        let Some(task) = self.tasks.get(id) else {
            return;
        };
        for key in keys {
            if let Some(value) = task.spec.labels.get(&key)
                && let Some(ids) = self
                    .labelled
                    .get_mut(&key)
                    .and_then(|values| values.get_mut(value))
            {
                ids.remove(id);
            }
        }
    }

    pub fn settled(&self) -> bool {
        self.tasks.values().all(Task::finished)
    }

    fn holder(&self, id: &TaskId, held: &ClaimId) -> Result<&Task, Rejection> {
        let task = self.tasks.get(id).ok_or(Rejection::UnknownTask)?;
        match &task.phase {
            Phase::Claimed { claim, .. } if claim == held => Ok(task),
            Phase::Claimed { .. } | Phase::Open | Phase::Blocked { .. } => Err(Rejection::NotHolder),
            _ => Err(Rejection::Terminal),
        }
    }

    pub fn check(&self, at_ms: u64, op: &Op) -> Result<(), Rejection> {
        match op {
            Op::Post { task } => {
                if self.tasks.contains_key(&task.id) && !mutated(Mutation::PostOverwrites) {
                    return Err(Rejection::Exists);
                }
                if let Some(parent) = &task.parent
                    && !self.tasks.contains_key(parent)
                {
                    return Err(Rejection::UnknownParent(parent.clone()));
                }
                match task
                    .needs
                    .iter()
                    .find(|need| !self.tasks.contains_key(*need))
                {
                    Some(need) => Err(Rejection::UnknownNeed(need.clone())),
                    None => Ok(()),
                }
            }
            Op::Claim { task, .. } => self.claimable(task, at_ms).map(drop),
            Op::Finish { task, .. } if mutated(Mutation::FinishIgnoresClaim) => {
                match self.tasks.get(task).map(|task| &task.phase) {
                    None => Err(Rejection::UnknownTask),
                    Some(phase) if phase.terminal() => Err(Rejection::Terminal),
                    Some(Phase::Open) => Err(Rejection::NotHolder),
                    Some(_) => Ok(()),
                }
            }
            Op::Renew { task, claim, .. }
            | Op::Release { task, claim, .. }
            | Op::Finish { task, claim, .. } => self.holder(task, claim).map(drop),
            Op::Cancel { task, .. } => match self.tasks.get(task) {
                None => Err(Rejection::UnknownTask),
                Some(task) if task.finished() => Err(Rejection::Terminal),
                Some(_) => Ok(()),
            },
            Op::Reply { task, .. } | Op::Relabel { task, .. } | Op::Rank { task, .. } => {
                self.tasks.get(task).map(drop).ok_or(Rejection::UnknownTask)
            }
            Op::Move { task, parent } => self.movable(task, parent.as_ref()),
            Op::Grant { .. } | Op::Revoke { .. } | Op::Charter {} => Ok(()),
            Op::Tasks(op) => self.check_tasks(at_ms, op),
            Op::Sync { sync } => self.check_sync(sync),
        }
    }

    pub fn children(&self, task: &TaskId) -> impl Iterator<Item = &TaskId> {
        self.children.get(task).into_iter().flatten()
    }

    fn movable(&self, task: &TaskId, parent: Option<&TaskId>) -> Result<(), Rejection> {
        match self.tasks.get(task) {
            None => return Err(Rejection::UnknownTask),
            Some(moving) if moving.finished() => return Err(Rejection::Terminal),
            Some(_) => {}
        }
        let Some(parent) = parent else {
            return Ok(());
        };
        match self.tasks.get(parent) {
            None => return Err(Rejection::UnknownParent(parent.clone())),
            Some(above) if above.finished() => return Err(Rejection::Terminal),
            Some(_) => {}
        }
        if mutated(Mutation::MoveAllowsCycle) {
            return Ok(());
        }
        let mut at = Some(parent);
        while let Some(id) = at {
            if id == task {
                return Err(Rejection::Tasks(Refusal::Cycle(task.0.clone())));
            }
            at = self.tasks.get(id).and_then(|above| above.spec.parent.as_ref());
        }
        Ok(())
    }

    pub fn apply(&mut self, seq: u64, body: &str) -> Result<(), BoardError> {
        if seq != self.next {
            return Err(BoardError::Corrupt {
                seq,
                detail: format!("expected row {}", self.next),
            });
        }
        let event: Event = serde_json::from_str(body).map_err(|error| BoardError::Corrupt {
            seq,
            detail: error.to_string(),
        })?;
        self.next = seq + 1;
        if self.seen.contains_key(&event.id) || !access::owns(&event.by, &event.id) {
            return Ok(());
        }
        self.now_ms = self.now_ms.max(event.at_ms);
        let verdict = self.admit(&event.by, event.at_ms, &event.op).map(|()| seq);
        self.seen.insert(event.id.clone(), verdict.clone());
        if verdict.is_ok() {
            self.access.record(&event.by, &event.id, &event.op);
            self.transition(seq, &event);
        }
        Ok(())
    }

    fn record(&mut self, seq: u64, event: &Event, task: &TaskId, change: Change) {
        self.observe(seq, event, task, &change);
        if !mutated(Mutation::ReadyIndexStale) {
            self.reindex(task);
        }
        self.entries.push(Entry {
            seq,
            event: event.id.clone(),
            at_ms: self.now_ms,
            task: task.clone(),
            change,
        });
    }

    fn transition(&mut self, seq: u64, event: &Event) {
        let now = self.now_ms;
        if let Op::Sync { sync } = &event.op {
            self.transition_sync(seq, event, sync);
            return;
        }
        let Some(id) = event.op.task().cloned() else {
            return;
        };
        let change = match &event.op {
            Op::Post { task } => {
                if let Some(parent) = &task.parent {
                    self.children
                        .entry(parent.clone())
                        .or_default()
                        .insert(task.id.clone());
                }
                for need in &task.needs {
                    self.dependents
                        .entry(need.clone())
                        .or_default()
                        .insert(task.id.clone());
                }
                self.tasks.insert(
                    task.id.clone(),
                    Task {
                        spec: task.clone(),
                        phase: Phase::Open,
                        attempts: 0,
                        replies: Vec::new(),
                        assignee: task.terms.assignee.clone(),
                        rank: None,
                    },
                );
                self.record(seq, event, &id, Change::Posted);
                let doomed = task
                    .parent
                    .iter()
                    .chain(&task.needs)
                    .find(|above| self.dooms(above))
                    .cloned();
                if let Some(above) = doomed
                    && !mutated(Mutation::PostIgnoresDoomedNeeds)
                {
                    self.cancel(seq, event, &id, format!("{above} did not finish"));
                }
                return;
            }
            Op::Claim { lease_ms, .. } => {
                let until_ms = now.saturating_add(*lease_ms);
                let claim = ClaimId(event.id.0.clone());
                let Some(task) = self.tasks.get_mut(&id) else {
                    return;
                };
                let took_over = match &task.phase {
                    Phase::Claimed { claim, .. } => Some(claim.clone()),
                    _ => None,
                };
                task.attempts += 1;
                task.phase = Phase::Claimed {
                    claim: claim.clone(),
                    worker: event.by.clone(),
                    until_ms,
                };
                Change::Claimed {
                    claim,
                    worker: event.by.clone(),
                    until_ms,
                    took_over,
                }
            }
            Op::Renew {
                claim, lease_ms, ..
            } => {
                let until_ms = now.saturating_add(*lease_ms);
                if let Some(Task {
                    phase:
                        Phase::Claimed {
                            until_ms: until, ..
                        },
                    ..
                }) = self.tasks.get_mut(&id)
                {
                    *until = until_ms;
                }
                Change::Renewed {
                    claim: claim.clone(),
                    until_ms,
                }
            }
            Op::Release { claim, note, .. } => {
                if let Some(task) = self.tasks.get_mut(&id) {
                    task.phase = Phase::Open;
                }
                Change::Released {
                    claim: claim.clone(),
                    note: note.clone(),
                }
            }
            Op::Finish { outcome, .. } => {
                if let Some(task) = self.tasks.get_mut(&id) {
                    task.phase = match outcome {
                        Outcome::Done(result) => Phase::Done {
                            result: result.clone(),
                        },
                        Outcome::Failed(error) => Phase::Failed {
                            error: error.clone(),
                        },
                    };
                }
                self.record(seq, event, &id, Change::Finished(outcome.clone()));
                if matches!(outcome, Outcome::Failed(_)) && self.cascades(&id) {
                    self.cascade(seq, event, &id);
                }
                return;
            }
            Op::Cancel { reason, .. } => {
                self.cancel(seq, event, &id, reason.clone());
                return;
            }
            Op::Reply { body, .. } => {
                if let Some(task) = self.tasks.get_mut(&id) {
                    task.replies.push((event.by.clone(), body.clone()));
                }
                Change::Replied {
                    by: event.by.clone(),
                    body: body.clone(),
                }
            }
            Op::Relabel { set, unset, .. } => {
                if !mutated(Mutation::LabelIndexStale) {
                    self.unlabel(&id, unset.iter().chain(set.keys()).cloned());
                }
                if let Some(task) = self.tasks.get_mut(&id) {
                    let labels = &mut task.spec.labels;
                    if !mutated(Mutation::RelabelKeepsUnset) {
                        labels.retain(|key, _| !unset.contains(key));
                    }
                    labels.extend(set.clone());
                }
                Change::Relabeled {
                    set: set.clone(),
                    unset: unset.clone(),
                }
            }
            Op::Rank { rank, .. } => {
                if let Some(task) = self.tasks.get_mut(&id) {
                    task.rank = *rank;
                }
                Change::Ranked { rank: *rank }
            }
            Op::Move { parent, .. } => {
                let Some(task) = self.tasks.get_mut(&id) else {
                    return;
                };
                let from = std::mem::replace(&mut task.spec.parent, parent.clone());
                if !mutated(Mutation::MoveKeepsChildren) {
                    if let Some(old) = &from
                        && let Some(below) = self.children.get_mut(old)
                    {
                        below.remove(&id);
                        if below.is_empty() {
                            self.children.remove(old);
                        }
                    }
                    if let Some(new) = parent {
                        self.children.entry(new.clone()).or_default().insert(id.clone());
                    }
                }
                Change::Reparented {
                    from,
                    to: parent.clone(),
                }
            }
            Op::Grant { .. } | Op::Revoke { .. } | Op::Charter {} => return,
            Op::Tasks(op) => {
                self.steer(seq, event, op);
                return;
            }
            Op::Sync { .. } => return,
        };
        self.record(seq, event, &id, change);
    }

    fn cancel(&mut self, seq: u64, event: &Event, id: &TaskId, reason: String) {
        let Some(task) = self.tasks.get_mut(id) else {
            return;
        };
        if task.finished() {
            return;
        }
        task.phase = Phase::Cancelled {
            reason: reason.clone(),
        };
        self.record(seq, event, id, Change::Cancelled { reason });
        self.cascade(seq, event, id);
    }

    fn cascade(&mut self, seq: u64, event: &Event, id: &TaskId) {
        let below = self
            .children
            .get(id)
            .into_iter()
            .chain(self.dependents.get(id))
            .flatten()
            .cloned()
            .collect::<BTreeSet<_>>();
        for next in below {
            self.cancel(seq, event, &next, format!("{id} did not finish"));
        }
    }
}

pub enum Step {
    Append { seq: u64, body: String },
    Done(Applied),
}

enum Attempt {
    Fresh,
    Appended {
        seq: u64,
        result: Result<(), StoreError>,
    },
}

pub struct Pending {
    event: Event,
    body: String,
    attempt: Attempt,
}

impl Fold {
    pub fn submit(&self, event: Event) -> Result<Pending, BoardError> {
        let body = serde_json::to_string(&event).map_err(|error| BoardError::Corrupt {
            seq: self.next,
            detail: error.to_string(),
        })?;
        Ok(Pending {
            event,
            body,
            attempt: Attempt::Fresh,
        })
    }
}

impl Pending {
    pub const fn event(&self) -> &Event {
        &self.event
    }

    pub fn read(
        &mut self,
        fold: &mut Fold,
        rows: Result<Vec<(u64, String)>, StoreError>,
    ) -> Result<Step, BoardError> {
        let attempt = std::mem::replace(&mut self.attempt, Attempt::Fresh);
        let rows = match (rows, &attempt) {
            (Ok(rows), _) => rows,
            (Err(error), Attempt::Appended { .. }) => {
                return Err(BoardError::Unsettled {
                    event: self.event.id.clone(),
                    error,
                });
            }
            (Err(error), _) => return Err(BoardError::Store(error)),
        };
        for (seq, body) in rows {
            fold.apply(seq, &body)?;
        }
        if let Some(verdict) = fold.outcome(&self.event.id) {
            return verdict
                .clone()
                .map(|seq| {
                    Step::Done(Applied {
                        seq,
                        event: self.event.id.clone(),
                    })
                })
                .map_err(BoardError::Rejected);
        }
        if let Attempt::Appended { seq, result } = attempt {
            match result {
                Ok(()) => {
                    return Err(BoardError::Corrupt {
                        seq,
                        detail: format!(
                            "the store acknowledged {} but holds another row",
                            self.event.id
                        ),
                    });
                }
                Err(error) if fold.next <= seq => {
                    return Err(BoardError::Unsettled {
                        event: self.event.id.clone(),
                        error,
                    });
                }
                Err(_) => {}
            }
        }
        fold.admit(&self.event.by, self.event.at_ms, &self.event.op)
            .map_err(BoardError::Rejected)?;
        Ok(Step::Append {
            seq: fold.next,
            body: self.body.clone(),
        })
    }

    pub fn appended(&mut self, seq: u64, result: Result<(), StoreError>) -> Result<(), BoardError> {
        if let Err(error) = &result
            && mutated(Mutation::ErrorsLoseWrites)
        {
            return Err(BoardError::Store(error.clone()));
        }
        self.attempt = Attempt::Appended { seq, result };
        Ok(())
    }
}
