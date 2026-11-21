use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::sim::{Mutation, mutated};
use crate::{Change, Entry, EventId, Fold, Op, TaskId};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Retain {
    pub settled: usize,
    pub entries: usize,
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
        self.retention.policy = policy;
        if policy.is_none() || had {
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
            if self.tasks.get(&id).is_some_and(|task| task.phase.terminal()) {
                self.retention.order.insert((seq, id.clone()));
                self.retention.settled.insert(id, seq);
            }
        }
    }

    pub const fn retain(&self) -> Option<Retain> {
        self.retention.policy
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
        }
        self.evict(seq);
    }

    fn settle(&mut self, seq: u64, id: &TaskId) {
        let terminal = self.tasks.get(id).is_some_and(|task| task.phase.terminal());
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
        let Some(policy) = self.retention.policy else {
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
                self.retention.posted.insert(entry.task.clone(), base + index);
            }
        }
        self.entries.drain(..over);
        self.retention.base += over;
    }

    pub(crate) fn restored_posts(&self) -> BTreeMap<TaskId, usize> {
        let base = self.retention.base;
        let mut posted = self.retention.trimmed_posts().clone();
        for (index, entry) in self.entries.iter().enumerate() {
            if entry.change == Change::Posted {
                posted.entry(entry.task.clone()).or_insert(base + index);
            }
        }
        posted
    }

    pub(crate) fn restore_retention(&mut self) {
        if mutated(Mutation::EvictionDiffersByReplica) && !self.retention.order.is_empty() {
            let (at, id) = self.retention.order.iter().next_back().cloned().expect("non-empty");
            self.retention.order.remove(&(at, id.clone()));
            self.retention.order.insert((0, id.clone()));
            self.retention.settled.insert(id, 0);
        }
    }
}
