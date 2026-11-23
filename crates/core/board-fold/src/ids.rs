//! A label index's set of task ids: most label values name one task (a job, a key), so a lone id is held inline
//! rather than in a B-tree node of its own (a BTreeSet's first insert allocates a whole leaf).
use std::collections::{BTreeSet, btree_set};

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::TaskId;

#[derive(Clone, Debug)]
pub(crate) enum Ids {
    One(TaskId),
    Many(BTreeSet<TaskId>),
}

impl Default for Ids {
    fn default() -> Self {
        Self::Many(BTreeSet::new())
    }
}

impl PartialEq for Ids {
    fn eq(&self, other: &Self) -> bool {
        self.iter().eq(other.iter())
    }
}

impl Eq for Ids {}

impl Ids {
    pub(crate) fn insert(&mut self, id: TaskId) {
        match self {
            Self::Many(set) if set.is_empty() => *self = Self::One(id),
            Self::Many(set) => {
                set.insert(id);
            }
            Self::One(one) if *one == id => {}
            Self::One(one) => {
                let one = std::mem::replace(one, TaskId(String::new()));
                *self = Self::Many(BTreeSet::from([one, id]));
            }
        }
    }

    pub(crate) fn remove(&mut self, id: &TaskId) {
        match self {
            Self::One(one) if one == id => *self = Self::default(),
            Self::One(_) => {}
            Self::Many(set) => {
                set.remove(id);
            }
        }
    }

    pub(crate) fn is_empty(&self) -> bool {
        matches!(self, Self::Many(set) if set.is_empty())
    }

    pub(crate) fn len(&self) -> usize {
        match self {
            Self::One(_) => 1,
            Self::Many(set) => set.len(),
        }
    }

    pub(crate) fn iter(&self) -> Iter<'_> {
        match self {
            Self::One(one) => Iter::One(Some(one)),
            Self::Many(set) => Iter::Many(set.iter()),
        }
    }
}

pub(crate) enum Iter<'a> {
    One(Option<&'a TaskId>),
    Many(btree_set::Iter<'a, TaskId>),
}

impl<'a> Iterator for Iter<'a> {
    type Item = &'a TaskId;

    fn next(&mut self) -> Option<&'a TaskId> {
        match self {
            Self::One(one) => one.take(),
            Self::Many(many) => many.next(),
        }
    }
}

impl<'a> IntoIterator for &'a Ids {
    type Item = &'a TaskId;
    type IntoIter = Iter<'a>;

    fn into_iter(self) -> Iter<'a> {
        self.iter()
    }
}

impl Serialize for Ids {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_seq(self.iter())
    }
}

impl<'de> Deserialize<'de> for Ids {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let mut ids = Self::default();
        for id in Vec::<TaskId>::deserialize(deserializer)? {
            ids.insert(id);
        }
        Ok(ids)
    }
}
