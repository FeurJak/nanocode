use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::sim::{Mutation, mutated};
use crate::{Change, Entry, EventId, Fold, Op, TaskId};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Retain {
    pub settled: usize,
    pub entries: usize,
    #[serde(default)]
    pub only: Option<(String, String)>,
    /// Events each task keeps (0: every one): past it the oldest of a task's events leave its dedupe
    /// record (seen), its replies and its said thread, so an open task that keeps hearing events
    /// holds a bounded number of them rather than every one since it was posted. A replay of an
    /// event trimmed here folds again; boards whose writers append each event once (a dense log)
    /// lose nothing by it.
    #[serde(default)]
    pub events: usize,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Retention {
    policy: Option<Retain>,
    base: usize,
    evicted: u64,
    #[serde(with = "crate::pairs")]
    settled: BTreeMap<TaskId, u64>,
    order: BTreeSet<(u64, TaskId)>,
    #[serde(with = "crate::pairs")]
    events: BTreeMap<TaskId, Vec<EventId>>,
    #[serde(with = "crate::pairs")]
    minted: BTreeMap<String, u64>,
    #[serde(with = "crate::pairs")]
    posted: BTreeMap<TaskId, usize>,
    gone: BTreeSet<TaskId>,
}

impl Retention {
    pub const fn new() -> Self {
        Self {
            policy: None,
            base: 0,
            evicted: 0,
            settled: BTreeMap::new(),
            order: BTreeSet::new(),
            events: BTreeMap::new(),
            minted: BTreeMap::new(),
            posted: BTreeMap::new(),
            gone: BTreeSet::new(),
        }
    }

    pub(crate) fn gone(&self, task: &TaskId) -> bool {
        self.gone.contains(task)
    }

    pub(crate) fn minted(&self, writer: &str) -> u64 {
        self.minted.get(writer).copied().unwrap_or(0)
    }

    pub(crate) const fn base(&self) -> usize {
        self.base
    }

    pub(crate) fn trimmed_posts(&self) -> &BTreeMap<TaskId, usize> {
        &self.posted
    }
}

impl Fold {
    pub fn retaining(policy: Retain) -> Self {
        let mut fold = Self::new();
        fold.retention.policy = Some(policy);
        fold
    }

    pub fn set_retain(&mut self, policy: Option<Retain>) {
        let had = self.retention.policy.is_some();
        let some = policy.is_some();
        self.retention.policy = policy;
        if !some || had {
            return;
        }
        let mut events: BTreeMap<TaskId, Vec<EventId>> = BTreeMap::new();
        let mut settled = BTreeMap::new();
        for entry in &self.entries {
            let list = events.entry(entry.task.clone()).or_default();
            if list.last() != Some(&entry.event) {
                list.push(entry.event.clone());
            }
            if entry.change.terminal() {
                settled.insert(entry.task.clone(), entry.seq);
            }
        }
        self.retention.events = events;
        self.retention.settled.clear();
        self.retention.order.clear();
        for (id, seq) in settled {
            if self.tracked(&id) {
                self.retention.order.insert((seq, id.clone()));
                self.retention.settled.insert(id, seq);
            }
        }
    }

    pub const fn retain(&self) -> Option<&Retain> {
        self.retention.policy.as_ref()
    }

    pub const fn entries_base(&self) -> usize {
        self.retention.base
    }

    pub const fn entry_count(&self) -> usize {
        self.retention.base + self.entries.len()
    }

    pub fn entries_since(&self, count: usize) -> &[Entry] {
        let from = count.saturating_sub(self.retention.base);
        self.entries.get(from..).unwrap_or(&[])
    }

    pub const fn evicted(&self) -> u64 {
        self.retention.evicted
    }

    pub fn settled_count(&self) -> usize {
        self.retention.settled.len()
    }

    pub(crate) fn heard(&mut self, event: &EventId, op: &Op) {
        if self.retention.policy.is_none() {
            return;
        }
        let Some(task) = op.task() else {
            return;
        };
        if self.tasks.contains_key(task) {
            let list = self.retention.events.entry(task.clone()).or_default();
            if list.last() != Some(event) {
                list.push(event.clone());
            }
        }
    }

    pub(crate) fn retained(&mut self, from: usize, seq: u64) {
        if self.retention.policy.is_none() {
            return;
        }
        let touched = self
            .entries_since(from)
            .iter()
            .map(|entry| (entry.task.clone(), entry.event.clone()))
            .collect::<Vec<_>>();
        for (id, event) in touched {
            if self.tasks.contains_key(&id) {
                let list = self.retention.events.entry(id.clone()).or_default();
                if list.last() != Some(&event) {
                    list.push(event);
                }
            }
            self.settle(seq, &id);
            self.cap(&id);
        }
        self.evict(seq);
    }

    /// Keep task's newest policy.events events: the older ones leave seen (raising their writer's
    /// minted floor, as an eviction does), and its replies and said thread keep as many.
    fn cap(&mut self, id: &TaskId) {
        let keep = self.retention.policy.as_ref().map_or(0, |policy| policy.events);
        if keep == 0 || mutated(Mutation::EventsUncapped) {
            return;
        }
        let Some(list) = self.retention.events.get_mut(id) else {
            return;
        };
        let over = list.len().saturating_sub(keep);
        if over == 0 {
            return;
        }
        for event in list.drain(..over).collect::<Vec<_>>() {
            self.seen.remove(&event);
            if let Some((writer, count)) = event.0.rsplit_once('.')
                && let Ok(count) = count.parse::<u64>()
            {
                let floor = self.retention.minted.entry(writer.to_owned()).or_default();
                *floor = (*floor).max(count);
            }
        }
        if let Some(task) = self.tasks.get_mut(id) {
            let over = task.replies.len().saturating_sub(keep);
            task.replies.drain(..over);
        }
        if !self.mirror.holds(id) {
            self.said.trim(&id.0, keep);
        }
    }

    fn settle(&mut self, seq: u64, id: &TaskId) {
        let terminal = self.tracked(id);
        match (terminal, self.retention.settled.get(id).copied()) {
            (true, None) => {
                self.retention.settled.insert(id.clone(), seq);
                self.retention.order.insert((seq, id.clone()));
            }
            (false, Some(at)) => {
                self.retention.settled.remove(id);
                self.retention.order.remove(&(at, id.clone()));
            }
            _ => {}
        }
    }

    fn tracked(&self, id: &TaskId) -> bool {
        let only = self
            .retention
            .policy
            .as_ref()
            .and_then(|policy| policy.only.as_ref());
        self.tasks.get(id).is_some_and(|task| {
            task.phase.terminal()
                && only.is_none_or(|(key, value)| task.spec.labels.get(key) == Some(value))
        })
    }

    fn evictable(&self, id: &TaskId) -> bool {
        let Some(task) = self.tasks.get(id) else {
            return false;
        };
        let goal_parent = task.spec.parent.as_ref().is_some_and(|parent| {
            self.intents.holds_goal(parent)
                && self.tasks.get(parent).is_some_and(|p| !p.phase.terminal())
        });
        task.phase.terminal()
            && task.spec.terms.recur.is_none()
            && !goal_parent
            && self.children.get(id).is_none_or(BTreeSet::is_empty)
            && self.dependents.get(id).is_none_or(BTreeSet::is_empty)
            && !self.mirror.holds(id)
            && !self.marks.lanes_hold(id)
            && !self.steers.holds(id)
    }

    fn evict(&mut self, _seq: u64) {
        let Some(policy) = self.retention.policy.clone() else {
            return;
        };
        let over = self.retention.settled.len().saturating_sub(policy.settled);
        if over > 0 {
            let doomed = self
                .retention
                .order
                .iter()
                .filter(|(_, id)| self.evictable(id))
                .take(over)
                .map(|(_, id)| id.clone())
                .collect::<Vec<_>>();
            for id in doomed {
                self.forget(&id);
            }
        }
        self.trim(policy.entries);
    }

    fn forget(&mut self, id: &TaskId) {
        let Some(task) = self.tasks.remove(id) else {
            return;
        };
        for (key, value) in &task.spec.labels {
            if let Some(values) = self.labelled.get_mut(key) {
                if let Some(ids) = values.get_mut(value) {
                    ids.remove(id);
                    if ids.is_empty() {
                        values.remove(value);
                    }
                }
                if values.is_empty() {
                    self.labelled.remove(key);
                }
            }
        }
        self.live.remove(id);
        if let Some(parent) = &task.spec.parent
            && let Some(below) = self.children.get_mut(parent)
        {
            below.remove(id);
            if below.is_empty() {
                self.children.remove(parent);
            }
        }
        self.children.remove(id);
        for need in &task.spec.needs {
            if let Some(above) = self.dependents.get_mut(need) {
                above.remove(id);
                if above.is_empty() {
                    self.dependents.remove(need);
                }
            }
        }
        self.dependents.remove(id);
        self.posted.remove(id);
        self.retention.posted.remove(id);
        self.access.forget(id);
        self.mirror.forget(id);
        self.said.forget(&id.0);
        self.intents.forget(id, task.spec.parent.as_ref());
        if let Some(at) = self.retention.settled.remove(id) {
            self.retention.order.remove(&(at, id.clone()));
        }
        for event in self.retention.events.remove(id).unwrap_or_default() {
            self.seen.remove(&event);
            if let Some((writer, count)) = event.0.rsplit_once('.')
                && let Ok(count) = count.parse::<u64>()
            {
                let floor = self.retention.minted.entry(writer.to_owned()).or_default();
                *floor = (*floor).max(count);
            }
        }
        self.retention.gone.insert(id.clone());
        self.retention.evicted += 1;
    }

    fn trim(&mut self, max: usize) {
        let over = self.entries.len().saturating_sub(max);
        if over == 0 {
            return;
        }
        let base = self.retention.base;
        for (index, entry) in self.entries[..over].iter().enumerate() {
            if entry.change == Change::Posted
                && self.posted.get(&entry.task) == Some(&(base + index))
            {
                self.retention
                    .posted
                    .insert(entry.task.clone(), base + index);
            }
        }
        self.entries.drain(..over);
        self.retention.base += over;
    }

    pub(crate) fn restored_posts(&self) -> BTreeMap<TaskId, usize> {
        let base = self.retention.base;
        let mut posted = self.retention.trimmed_posts().clone();
        for (index, entry) in self.entries.iter().enumerate() {
            if entry.change == Change::Posted && self.tasks.contains_key(&entry.task) {
                posted.entry(entry.task.clone()).or_insert(base + index);
            }
        }
        posted
    }

    pub(crate) fn restore_retention(&mut self) {
        if mutated(Mutation::EvictionDiffersByReplica) && !self.retention.order.is_empty() {
            let (at, id) = self
                .retention
                .order
                .iter()
                .next_back()
                .cloned()
                .expect("non-empty");
            self.retention.order.remove(&(at, id.clone()));
            self.retention.order.insert((0, id.clone()));
            self.retention.settled.insert(id, 0);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use crate::access::{Can, Grant, Who};
    use crate::tasks::Terms;
    use crate::{Event, EventId, Fold, NewTask, Op, Rejection, Retain, TaskId};

    fn write(fold: &mut Fold, minted: &mut u64, op: Op) -> Result<u64, Rejection> {
        *minted += 1;
        let id = EventId(format!("user:lead.{minted}"));
        let body = serde_json::to_string(&Event {
            id: id.clone(),
            by: "user:lead".to_owned(),
            at_ms: *minted,
            op,
        })
        .expect("an event encodes");
        fold.apply(fold.next(), &body).expect("the row applies");
        fold.outcome(&id).cloned().expect("an outcome")
    }

    fn post(id: &str, kind: &str) -> Op {
        Op::Post {
            task: NewTask {
                id: TaskId(id.to_owned()),
                parent: None,
                spec: id.to_owned(),
                labels: BTreeMap::from([("kind".to_owned(), kind.to_owned())]),
                needs: Vec::new(),
                terms: Terms::default(),
            },
        }
    }

    fn cancel(id: &str) -> Op {
        Op::Cancel {
            task: TaskId(id.to_owned()),
            reason: "done".to_owned(),
        }
    }

    fn board(policy: Retain, tasks: usize) -> Fold {
        let mut fold = Fold::retaining(policy);
        let mut minted = 0;
        assert!(write(&mut fold, &mut minted, Op::Charter {}).is_ok());
        let grant = Grant {
            who: vec![Who::Everyone],
            can: BTreeMap::from([("*".to_owned(), Can::ALL.to_vec())]),
        };
        assert!(write(&mut fold, &mut minted, Op::Grant { grant }).is_ok());
        for n in 0..tasks {
            let kind = if n % 2 == 0 { "land" } else { "thread" };
            let id = format!("t{n}");
            assert!(write(&mut fold, &mut minted, post(&id, kind)).is_ok());
            assert!(write(&mut fold, &mut minted, cancel(&id)).is_ok());
        }
        fold
    }

    #[test]
    fn an_open_task_keeps_its_newest_events_past_the_policy() {
        let policy = Retain {
            settled: 8,
            entries: 1_000,
            only: None,
            events: 4,
        };
        let mut fold = board(policy, 0);
        let mut minted = 2;
        assert!(write(&mut fold, &mut minted, post("open", "thread")).is_ok());
        let posted = EventId(format!("user:lead.{minted}"));
        for n in 0..50 {
            let reply = Op::Reply {
                task: TaskId("open".to_owned()),
                body: format!("line {n}"),
            };
            assert!(write(&mut fold, &mut minted, reply).is_ok());
        }
        let task = fold.task(&TaskId("open".to_owned())).expect("the open task stays");
        assert!(!task.phase.terminal());
        let bodies: Vec<&str> = task.replies.iter().map(|(_, body)| body.as_str()).collect();
        assert_eq!(bodies, ["line 46", "line 47", "line 48", "line 49"]);
        assert!(fold.outcome(&posted).is_none(), "its post left seen");
        let last = EventId(format!("user:lead.{minted}"));
        assert!(fold.outcome(&last).is_some(), "its newest event stays");
        assert_eq!(fold.minted("user:lead"), minted, "the writer's floor holds");
        assert_eq!(fold.said.replies[&crate::messages::MessageId("open".to_owned())].len(), 4);
        let restored = Fold::restore(&fold.snapshot()).expect("a snapshot restores");
        assert_eq!(restored, fold);
    }

    #[test]
    fn evicts_the_oldest_settled_past_the_policy() {
        let policy = Retain {
            settled: 3,
            entries: 8,
            only: None,
            events: 0,
        };
        let mut fold = board(policy, 20);
        assert_eq!(
            fold.tasks().len(),
            3,
            "{:?}",
            fold.tasks().keys().collect::<Vec<_>>()
        );
        assert!(fold.task(&TaskId("t19".to_owned())).is_some());
        assert!(fold.task(&TaskId("t0".to_owned())).is_none());
        assert_eq!(fold.evicted(), 17);
        assert!(fold.entries().len() <= 8);
        assert_eq!(
            fold.entry_count(),
            fold.entries_base() + fold.entries().len()
        );
        let restored = Fold::restore(&fold.snapshot()).expect("restores");
        assert_eq!(restored, fold);
        let json = Fold::restore(fold.snapshot_json().as_bytes()).expect("restores");
        assert_eq!(json, fold);
        let mut minted = 1_000;
        assert_eq!(
            write(&mut fold, &mut minted, post("t0", "land")),
            Err(Rejection::Exists)
        );
    }

    #[test]
    fn only_evicts_tasks_with_its_label() {
        let only = Some(("kind".to_owned(), "land".to_owned()));
        let fold = board(
            Retain {
                settled: 2,
                entries: 1_000,
                only,
            },
            20,
        );
        let land = fold
            .tasks()
            .values()
            .filter(|t| t.spec.labels["kind"] == "land")
            .count();
        assert_eq!(land, 2);
        assert_eq!(fold.tasks().len(), 12);
    }
}
