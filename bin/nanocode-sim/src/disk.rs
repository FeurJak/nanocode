use std::{
    collections::{BTreeMap, BTreeSet},
    ops::Range,
    sync::Arc,
};

use nanocode_sync::Slot;
use nanocode_turnlog::{Compaction, Extension, Header, Image, ImageOptions, Medium};

pub type Compacted = (String, Vec<(u64, String)>, Range<u64>);

pub enum FileOp {
    Append {
        archived: Vec<(u64, String)>,
        dropped: Range<u64>,
        rows: Vec<(u64, String)>,
        scope: String,
        superseded: Vec<Range<u64>>,
    },
    Extend {
        cut: usize,
        row: u64,
        scope: String,
        seq: u64,
        tail: String,
    },
}

#[derive(Default)]
pub struct Rows {
    pub appends: Vec<(u64, String, String)>,
    pub log: BTreeMap<u64, (String, String)>,
    pub page_reads: u64,
    pub reads: u64,
    pub snaps: BTreeMap<String, (u64, String)>,
    pub staged: Vec<(String, u64, String)>,
    pub tappends: Vec<(String, u64, String)>,
    pub tcompactions: Vec<Compacted>,
    pub tfiles: BTreeMap<String, Image<Vec<u8>>>,
    pub tlog: BTreeMap<(String, u64), String>,
    pub tops: Vec<FileOp>,
    pub tprior: BTreeMap<String, Header>,
}

pub fn compact(rows: &mut BTreeMap<u64, String>, archived: &[(u64, String)], dropped: &Range<u64>) {
    for (seq, body) in archived {
        assert!(
            rows.insert(*seq, body.clone()).is_some(),
            "an archived turn log row {seq} replaces no stored row"
        );
    }
    for seq in dropped.clone() {
        assert!(
            rows.remove(&seq).is_some(),
            "a dropped turn log row {seq} is not stored"
        );
    }
}

fn count(len: usize) -> u64 {
    u64::try_from(len).expect("a row count fits in u64")
}

impl Rows {
    pub fn write_files(&mut self, ops: Vec<FileOp>) -> Result<(), String> {
        for op in ops {
            let scope = match &op {
                FileOp::Append { scope, .. } | FileOp::Extend { scope, .. } => scope.clone(),
            };
            let image = match self.tfiles.entry(scope.clone()) {
                std::collections::btree_map::Entry::Occupied(entry) => entry.into_mut(),
                std::collections::btree_map::Entry::Vacant(entry) => entry.insert(
                    Image::open(Vec::new(), Vec::new(), ImageOptions::default())
                        .map_err(|error| format!("turn log file {scope} does not open: {error}"))?,
                ),
            };
            let before = image.header();
            match &op {
                FileOp::Append {
                    archived,
                    dropped,
                    rows,
                    superseded,
                    ..
                } => image.append(
                    rows,
                    &Compaction {
                        archived,
                        dropped: dropped.clone(),
                        superseded,
                    },
                ),
                FileOp::Extend {
                    cut,
                    row,
                    seq,
                    tail,
                    ..
                } => image.extend(&Extension {
                    cut: *cut,
                    row: *row,
                    seq: *seq,
                    tail,
                }),
            }
            .map_err(|error| format!("turn log file {scope} rejects a committed write: {error}"))?;
            if image.header().snapshot != before.snapshot {
                self.tprior.insert(scope, before);
            }
        }
        Ok(())
    }

    pub fn reopen_files(&mut self, stale: bool) -> Result<(), String> {
        let scopes: Vec<String> = self.tfiles.keys().cloned().collect();
        for scope in scopes {
            let Some(image) = self.tfiles.get(&scope) else {
                continue;
            };
            let mut data = image.data().clone();
            let header = image.header();
            if stale && let Some(prior) = self.tprior.get(&scope) {
                let pointer = Header {
                    snapshot: prior.snapshot,
                    tail: prior.tail,
                    ..header
                };
                data.write_at(pointer.offset(), &pointer.encode())
                    .map_err(|error| format!("turn log file {scope} header write: {error}"))?;
            }
            let mut reopened = Image::open(data, image.index().clone(), ImageOptions::default())
                .map_err(|error| format!("turn log file {scope} does not reopen: {error}"))?;
            let full = reopened
                .replay()
                .map_err(|error| format!("turn log file {scope} full replay: {error}"))?;
            let tail = reopened.load().map_err(|error| {
                format!("turn log file {scope} header and tail replay: {error}")
            })?;
            let mut stored = self.committed(&scope);
            let start = stored
                .iter()
                .rposition(|(_, body)| body.starts_with('s'))
                .unwrap_or(0);
            let stored = stored.split_off(start);
            if tail != full || tail != stored {
                return Err(format!(
                    "turn log file {scope} header and tail replay holds rows {:?}, full replay {:?}, store {:?}",
                    tail.iter().map(|(seq, _)| *seq).collect::<Vec<_>>(),
                    full.iter().map(|(seq, _)| *seq).collect::<Vec<_>>(),
                    stored.iter().map(|(seq, _)| *seq).collect::<Vec<_>>()
                ));
            }
            self.tfiles.insert(scope, reopened);
        }
        Ok(())
    }

    pub fn append(&mut self, scope: &str, body: String) -> u64 {
        let committed = self.log.keys().next_back().copied().unwrap_or(0);
        let pending = self.appends.last().map_or(0, |(seq, ..)| *seq);
        let seq = committed.max(pending) + 1;
        self.appends.push((seq, scope.to_owned(), body));
        seq
    }

    pub fn snapshot(&mut self, scope: &str) -> Option<(u64, String)> {
        let snap = self
            .staged
            .iter()
            .rev()
            .find(|(owner, ..)| owner == scope)
            .map(|(_, seq, body)| (*seq, body.clone()))
            .or_else(|| self.snaps.get(scope).cloned());
        self.reads += u64::from(snap.is_some());
        snap
    }

    pub fn scan(&mut self, scope: &str, after: u64) -> Vec<(u64, String)> {
        let rows: Vec<(u64, String)> = self
            .log
            .range(after + 1..)
            .map(|(seq, (owner, body))| (*seq, owner, body))
            .chain(
                self.appends
                    .iter()
                    .map(|(seq, owner, body)| (*seq, owner, body)),
            )
            .filter(|(seq, owner, _)| *seq > after && owner.as_str() == scope)
            .map(|(seq, _, body)| (seq, body.clone()))
            .collect();
        self.reads += count(rows.len());
        rows
    }

    pub fn stage(&mut self, scope: &str, seq: u64, body: String) {
        self.staged.push((scope.to_owned(), seq, body));
    }

    pub fn insert(&mut self, scope: &str, rows: &[(u64, String)]) -> bool {
        let taken = rows.iter().any(|(seq, _)| {
            self.tlog.contains_key(&(scope.to_owned(), *seq))
                || self
                    .tappends
                    .iter()
                    .any(|(owner, pending, _)| owner == scope && pending == seq)
        });
        if !taken {
            self.tappends.extend(
                rows.iter()
                    .map(|(seq, body)| (scope.to_owned(), *seq, body.clone())),
            );
        }
        !taken
    }

    pub fn committed(&self, scope: &str) -> Vec<(u64, String)> {
        self.tlog
            .range((scope.to_owned(), 0)..=(scope.to_owned(), u64::MAX))
            .map(|((_, seq), body)| (*seq, body.clone()))
            .collect()
    }

    pub fn view(&self, scope: &str) -> Vec<(u64, String)> {
        let mut rows = self
            .committed(scope)
            .into_iter()
            .chain(
                self.tappends
                    .iter()
                    .filter(|(owner, ..)| owner == scope)
                    .map(|(_, seq, body)| (*seq, body.clone())),
            )
            .collect::<BTreeMap<_, _>>();
        for (owner, archived, dropped) in &self.tcompactions {
            if owner == scope {
                compact(&mut rows, archived, dropped);
            }
        }
        rows.into_iter().collect()
    }

    pub fn tscan(&mut self, scope: &str, from: u64, to: u64) -> Vec<(u64, String)> {
        let mut rows = self.view(scope);
        rows.retain(|(seq, _)| (from..to).contains(seq));
        self.page_reads += count(rows.len());
        rows
    }

    pub fn tload(&mut self, scope: &str) -> Vec<(u64, String)> {
        let mut rows = self.view(scope);
        let start = rows
            .iter()
            .rposition(|(_, body)| body.starts_with('s'))
            .unwrap_or(0);
        let rows = rows.split_off(start);
        self.reads += count(rows.len());
        rows
    }

    pub fn scopes(&self) -> BTreeSet<String> {
        self.log
            .values()
            .map(|(scope, _)| scope.clone())
            .chain(self.snaps.keys().cloned())
            .chain(self.tlog.keys().map(|(scope, _)| scope.clone()))
            .collect()
    }
}

pub struct Disk(Slot<Rows>);

impl Disk {
    pub fn new() -> Arc<Self> {
        Arc::new(Self(Slot::new(Some(Rows::default()))))
    }

    pub fn with<R>(&self, change: impl FnOnce(&mut Rows) -> R) -> R {
        let mut rows = self
            .0
            .take()
            .expect("the simulator runs one tick at a time, so its disk is never taken twice");
        let result = change(&mut rows);
        assert!(
            self.0.put(rows).is_none(),
            "the simulated disk has one home"
        );
        result
    }
}
