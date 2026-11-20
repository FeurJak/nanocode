use std::collections::VecDeque;
use std::sync::{Arc, OnceLock};

use crate::seal::{Encoded, Seal, outline_ops};
use crate::sim::{Mutation, mutated};
use crate::table::Table;
use crate::{Access, Cursor, Diff, Op, Row, Snapshot, ViewKey};

pub trait View {
    type Record: ?Sized;

    fn apply(&mut self, pos: u64, record: &Self::Record, ops: &mut Vec<Op>);

    fn rows(&self) -> Vec<Row>;
}

#[derive(Debug)]
pub struct Entry {
    diff: Diff,
    full: OnceLock<Arc<Encoded>>,
    outline: OnceLock<Arc<Encoded>>,
}

impl Entry {
    pub const fn new(diff: Diff) -> Self {
        Self {
            diff,
            full: OnceLock::new(),
            outline: OnceLock::new(),
        }
    }

    pub const fn diff(&self) -> &Diff {
        &self.diff
    }

    pub fn frame<S: Seal + ?Sized>(&self, access: Access, seal: &S) -> Option<Arc<Encoded>> {
        let access = match access {
            Access::Outline if mutated(Mutation::OutlineLeaks) => Access::Full,
            access => access,
        };
        let (cell, ops) = match access {
            Access::Denied => return None,
            Access::Full => (&self.full, None),
            Access::Outline => (&self.outline, Some(seal)),
        };
        let encode = || {
            let diff = &self.diff;
            Arc::new(match ops {
                Some(seal) => Encoded::diff(
                    &diff.from,
                    &diff.to,
                    &outline_ops(seal, &diff.view, &diff.ops),
                ),
                None => Encoded::diff(&diff.from, &diff.to, &diff.ops),
            })
        };
        if mutated(Mutation::EncodeEveryFrame) {
            return Some(encode());
        }
        Some(Arc::clone(cell.get_or_init(encode)))
    }
}

#[derive(Debug)]
pub enum Resume {
    Diffs(Vec<Arc<Entry>>),
    Snapshot,
    Advance,
    Ahead,
}

#[derive(Debug)]
pub struct Ring {
    view: ViewKey,
    capacity: usize,
    floor: Cursor,
    entries: VecDeque<Arc<Entry>>,
}

impl Ring {
    pub const fn new(view: ViewKey, capacity: usize, floor: Cursor) -> Self {
        Self {
            view,
            capacity,
            floor,
            entries: VecDeque::new(),
        }
    }

    pub const fn floor(&self) -> Cursor {
        self.floor
    }

    pub fn tip(&self) -> Cursor {
        self.entries
            .back()
            .map_or(self.floor, |entry| entry.diff.to)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn entries(&self) -> impl Iterator<Item = &Arc<Entry>> {
        self.entries.iter()
    }

    pub fn push(&mut self, entry: Arc<Entry>) {
        assert_eq!(entry.diff.from, self.tip(), "a diff extends the ring's tip");
        assert!(
            entry.diff.to.pos >= entry.diff.from.pos,
            "a diff moves forward"
        );
        self.entries.push_back(entry);
        while self.entries.len() > self.capacity {
            if let Some(oldest) = self.entries.pop_front() {
                self.floor = oldest.diff.to;
            }
        }
    }

    pub fn resume(&self, cursor: Option<&Cursor>) -> Resume {
        let Some(cursor) = cursor else {
            return Resume::Snapshot;
        };
        let tip = self.tip();
        if cursor.partition != tip.partition {
            return Resume::Snapshot;
        }
        if cursor.pos < self.floor.pos && !mutated(Mutation::FloorIgnored) {
            return Resume::Snapshot;
        }
        if cursor.pos > tip.pos {
            return Resume::Ahead;
        }
        let mut diffs: Vec<Arc<Entry>> = self
            .entries
            .iter()
            .filter(|entry| entry.diff.to.pos > cursor.pos)
            .cloned()
            .collect();
        if mutated(Mutation::ResumeSkipsFirst) && diffs.len() > 1 {
            diffs.remove(0);
        }
        match diffs.first_mut() {
            Some(first) if first.diff.from != *cursor && !mutated(Mutation::RebaseSkipped) => {
                *first = Arc::new(Entry::new(Diff {
                    from: *cursor,
                    ..first.diff.clone()
                }));
            }
            Some(_) => {}
            None => diffs.push(Arc::new(Entry::new(Diff {
                view: self.view.clone(),
                from: *cursor,
                to: tip,
                ops: Vec::new(),
            }))),
        }
        Resume::Diffs(diffs)
    }
}

#[derive(Debug)]
pub struct Live<V> {
    key: ViewKey,
    view: V,
    table: Table,
    head: Cursor,
    ring: Ring,
    scratch: Vec<Op>,
}

impl<V: View> Live<V> {
    pub fn new(key: ViewKey, view: V, at: Cursor, capacity: usize) -> Self {
        let table = Table::from_rows(view.rows());
        let ring = Ring::new(key.clone(), capacity, at);
        Self {
            key,
            view,
            table,
            head: at,
            ring,
            scratch: Vec::new(),
        }
    }

    pub const fn key(&self) -> &ViewKey {
        &self.key
    }

    pub const fn view(&self) -> &V {
        &self.view
    }

    pub const fn table(&self) -> &Table {
        &self.table
    }

    pub const fn ring(&self) -> &Ring {
        &self.ring
    }

    pub const fn head(&self) -> Cursor {
        self.head
    }

    pub fn tip(&self) -> Cursor {
        self.ring.tip()
    }

    pub const fn begin(&mut self) -> Batch<'_, V> {
        Batch { live: self }
    }

    pub fn advance(&mut self) -> Option<Arc<Entry>> {
        let tip = self.tip();
        if tip == self.head {
            return None;
        }
        let entry = Arc::new(Entry::new(Diff {
            view: self.key.clone(),
            from: tip,
            to: self.head,
            ops: Vec::new(),
        }));
        self.ring.push(Arc::clone(&entry));
        Some(entry)
    }

    pub fn snapshot(&self) -> Snapshot {
        let mut rows = self.table.rows();
        if mutated(Mutation::SnapshotSkipsRow) {
            rows.pop();
        }
        let cursor = if mutated(Mutation::SnapshotAtHead) {
            self.head
        } else {
            self.tip()
        };
        Snapshot {
            view: self.key.clone(),
            cursor,
            rows,
        }
    }

    pub fn resume(&self, cursor: Option<&Cursor>) -> Resume {
        match self.ring.resume(cursor) {
            Resume::Ahead if cursor.is_some_and(|cursor| cursor.pos <= self.head.pos) => {
                Resume::Advance
            }
            resume => resume,
        }
    }
}

pub struct Batch<'a, V> {
    live: &'a mut Live<V>,
}

impl<V: View> Batch<'_, V> {
    pub fn fold(&mut self, at: Cursor, record: &V::Record) -> bool {
        let live = &mut *self.live;
        assert_eq!(
            at.partition, live.head.partition,
            "a view folds one partition"
        );
        if at.pos <= live.head.pos {
            return false;
        }
        live.view.apply(at.pos, record, &mut live.scratch);
        for op in live.scratch.drain(..) {
            live.table.set(op);
        }
        live.head = at;
        true
    }

    pub fn finish(self) -> Option<Arc<Entry>> {
        let live = self.live;
        let ops = live.table.settle();
        if ops.is_empty() {
            return None;
        }
        let entry = Arc::new(Entry::new(Diff {
            view: live.key.clone(),
            from: live.tip(),
            to: live.head,
            ops,
        }));
        live.ring.push(Arc::clone(&entry));
        Some(entry)
    }
}
