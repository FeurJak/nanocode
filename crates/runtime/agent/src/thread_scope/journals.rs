use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    sync::Arc,
};

use nanocode_sync::Atom;
use nanocode_tool_host::{
    code_mode::{
        CellJournal, CellJournalSink, CellJournalUpdate, CellJournals, JournalEntry,
        JournaledCallState,
    },
    embedded::HostFuture,
};
use nanocode_turnlog::{EffectClass, Effects, Record};
use serde_json::{Value, json, value::RawValue};

use super::{Staged, ThreadLog, ThreadState, facts::refused, sim, state::invalid};
use crate::{
    durability::StoreError,
    one_log::{STORED_MARK, TOOLS_MARK, raw, shared, stored_key},
};

type Cell = (String, u64);

struct Inner {
    log: Arc<ThreadLog>,
    effects: Arc<Atom<Effects>>,
    live: Atom<BTreeSet<Cell>>,
    unsaved: Atom<BTreeMap<Cell, u64>>,
    answered: Atom<BTreeMap<Cell, u64>>,
    dispatched: Atom<BTreeMap<Cell, BTreeSet<u64>>>,
}

#[derive(Clone)]
pub struct ThreadJournals(Arc<Inner>);

impl ThreadJournals {
    #[must_use]
    pub fn new(log: Arc<ThreadLog>, effects: Arc<Atom<Effects>>) -> Self {
        Self(Arc::new(Inner {
            log,
            effects,
            live: Atom::new(BTreeSet::new()),
            unsaved: Atom::new(BTreeMap::new()),
            answered: Atom::new(BTreeMap::new()),
            dispatched: Atom::new(BTreeMap::new()),
        }))
    }

    #[must_use]
    pub fn log(&self) -> &Arc<ThreadLog> {
        &self.0.log
    }

    #[must_use]
    pub fn live(&self) -> Vec<(String, u64)> {
        self.0.live.read(|live| live.iter().cloned().collect())
    }

    #[must_use]
    pub fn sink(&self) -> Arc<dyn CellJournalSink> {
        Arc::new(self.clone())
    }

    #[must_use]
    pub fn journals(&self) -> CellJournals {
        CellJournals::with_sink(self.sink())
    }

    async fn read<T: Send + 'static>(
        &self,
        read: impl Fn(&ThreadState) -> Result<T, StoreError> + Send + Sync + 'static,
    ) -> Result<T, StoreError> {
        self.0
            .log
            .job_staged(move |staged| read(staged.state()))
            .await
            .map_err(refused)
    }
}

fn staged_update(staged: &mut Staged, update: &CellJournalUpdate) -> Result<bool, StoreError> {
    let mut update = update.clone();
    let fresh = update.header.is_some();
    if let Some(header) = update.header.as_mut() {
        let catalog = serde_json::to_string(&header.tools)
            .map_err(|error| invalid(format!("tool catalog: {error}")))?;
        let digest = nanocode_digest::blake3(catalog.as_bytes())
            .to_hex()
            .to_string();
        let key = stored_key(TOOLS_MARK, &digest);
        if staged.state().stored(&key).is_none() {
            staged.push(Record::Stored {
                key,
                value: catalog,
            })?;
        }
        header.tools = [json!({ (TOOLS_MARK): digest })].into();
    }
    let encoded = match shared::shared_results(&update)? {
        Some(shared) => {
            for (key, value) in shared.values {
                let key = stored_key(&update.session_id, &key);
                if staged.state().stored(&key) != Some(&*value) {
                    staged.push(Record::Stored { key, value })?;
                }
            }
            shared.update
        }
        None => raw(&update)?,
    };
    staged.push(Record::Cell {
        cell: update.cell_id,
        fresh,
        session: update.session_id.clone(),
        update: encoded,
    })?;
    Ok(fresh)
}

struct Saved<'a> {
    update: &'a CellJournalUpdate,
    cell: Cell,
    fresh: bool,
    seq: u64,
    started: Vec<u64>,
    external: bool,
}

impl Inner {
    fn needs_row(&self, tool: &str) -> bool {
        self.effects.read(|effects| effects.of(tool)).needs_row()
    }

    fn external(&self, update: &CellJournalUpdate) -> Result<Vec<Record>, StoreError> {
        update
            .calls
            .iter()
            .filter(|call| {
                matches!(call.state, JournaledCallState::Running) && self.needs_row(&call.name)
            })
            .map(|call| {
                Ok(Record::Call {
                    call_id: call.call_id.clone(),
                    effect: EffectClass::External,
                    input: raw(&call.input)?,
                    tool: call.name.clone(),
                    turn: None,
                })
            })
            .collect()
    }

    fn due(&self, saved: &Saved<'_>) -> bool {
        let Saved {
            update,
            cell,
            fresh,
            seq,
            started,
            external,
        } = saved;
        if update.finished {
            self.live.update(|live| live.remove(cell));
        } else if *fresh {
            self.live.update(|live| live.insert(cell.clone()));
        }
        let timed = update
            .entries
            .iter()
            .any(|entry| matches!(entry, JournalEntry::Timeout { .. }));
        let pending = self.unsaved.read(|unsaved| unsaved.get(cell) == Some(seq));
        let results = update
            .entries
            .iter()
            .filter_map(|entry| match entry {
                JournalEntry::Result { id, .. } => Some(*id),
                _ => None,
            })
            .collect::<Vec<_>>();
        let answered = self.dispatched.update(|dispatched| {
            let calls = dispatched.entry(cell.clone()).or_default();
            calls.extend(started.iter().copied());
            let before = calls.len();
            calls.retain(|id| !results.contains(id));
            let answered = calls.len() < before;
            if calls.is_empty() || update.finished {
                dispatched.remove(cell);
            }
            answered
        });
        let owed = self.answered.read(|owed| owed.get(cell) == Some(seq));
        let due = *external
            || owed
            || (answered && (timed || update.finished))
            || (timed && (*fresh || pending));
        if due {
            self.unsaved.update(|unsaved| unsaved.remove(cell));
            self.answered.update(|owed| owed.remove(cell));
            return true;
        }
        self.unsaved.update(|unsaved| {
            if update.finished {
                unsaved.remove(cell);
            } else if *fresh {
                unsaved.insert(cell.clone(), *seq);
            }
        });
        self.answered.update(|owed| {
            if update.finished {
                owed.remove(cell);
            } else if answered {
                owed.insert(cell.clone(), *seq);
            }
        });
        false
    }

    async fn save(self: Arc<Self>, update: CellJournalUpdate) -> Result<(), StoreError> {
        let external = self.external(&update)?;
        let started = update
            .calls
            .iter()
            .filter(|call| {
                matches!(call.state, JournaledCallState::Running) && self.needs_row(&call.name)
            })
            .map(|call| call.id)
            .collect::<Vec<_>>();
        let update = Arc::new(update);
        let fresh = {
            let update = Arc::clone(&update);
            self.log
                .job_staged(move |staged| staged_update(staged, &update))
                .await
                .map_err(refused)?
        };
        let seq = self.log.next().ok_or_else(|| {
            refused(nanocode_scope::ScopeError::Routed {
                holder: self.log.holder().unwrap_or_default().to_owned(),
            })
        })?;
        let saved = Saved {
            update: &update,
            cell: (update.session_id.clone(), update.cell_id),
            fresh,
            seq,
            started,
            external: !external.is_empty(),
        };
        if !self.due(&saved) {
            return Ok(());
        }
        if external.is_empty() {
            self.log.flush(None).await.map(drop).map_err(refused)
        } else {
            self.log.write(external).await.map_err(refused)
        }
    }
}

fn restored(state: &ThreadState, session: &str, whole: &BTreeSet<u64>, shared: &[String]) {
    let (mut cells, mut finished, mut updates, mut finished_updates) = (0_u64, 0_u64, 0_u64, 0_u64);
    for (cell, held) in state.cells_of(session) {
        let bytes = held
            .iter()
            .map(|update| update.get().len() as u64)
            .sum::<u64>();
        cells += 1;
        updates += bytes;
        if !whole.contains(&cell) {
            finished += 1;
            finished_updates += bytes;
        }
    }
    let results = shared
        .iter()
        .filter_map(|key| state.stored(key))
        .map(|value| value.len() as u64)
        .sum::<u64>();
    nanocode_trace::event!(target: "nc_metrics", nanocode_trace::Level::INFO, {
        monotonic_counter.nc.cells.restored = cells,
        monotonic_counter.nc.cells.restored_finished = finished,
        monotonic_counter.nc.cells.restored_results = shared.len() as u64,
        histogram.nc.cells.restore_update_bytes = updates,
        histogram.nc.cells.restore_finished_update_bytes = finished_updates,
        histogram.nc.cells.restore_result_bytes = results
    });
}

fn catalog(
    state: &ThreadState,
    catalogs: &mut HashMap<String, Arc<[Value]>>,
    digest: &str,
) -> Result<Arc<[Value]>, StoreError> {
    if let Some(tools) = catalogs.get(digest) {
        return Ok(Arc::clone(tools));
    }
    let held = state
        .stored(&stored_key(TOOLS_MARK, digest))
        .ok_or_else(|| invalid(format!("tool catalog {digest}")))?;
    let tools: Arc<[Value]> = serde_json::from_str::<Vec<Value>>(held)
        .map_err(|error| invalid(format!("tool catalog: {error}")))?
        .into();
    catalogs.insert(digest.to_owned(), Arc::clone(&tools));
    Ok(tools)
}

fn decoded(
    state: &ThreadState,
    session: &str,
    loaded: bool,
    updates: &[Box<RawValue>],
    catalogs: &mut HashMap<String, Arc<[Value]>>,
) -> Result<Vec<CellJournalUpdate>, StoreError> {
    let mut decoded = Vec::with_capacity(updates.len());
    for update in updates {
        let update = if loaded {
            shared::with_results(state, session, update)?
        } else {
            shared::without_results(update)?
        };
        let mut update = serde_json::from_str::<CellJournalUpdate>(&update)
            .map_err(|error| invalid(format!("cell journal update: {error}")))?;
        if let Some(header) = update.header.as_mut()
            && let [Value::Object(mark)] = &*header.tools
            && let Some(digest) = mark.get(TOOLS_MARK).and_then(Value::as_str)
        {
            header.tools = catalog(state, catalogs, digest)?;
        }
        decoded.push(update);
    }
    Ok(decoded)
}

fn cells(
    state: &ThreadState,
    session: &str,
    only: Option<u64>,
) -> Result<Vec<CellJournal>, StoreError> {
    let held: Vec<(u64, Vec<Box<RawValue>>)> = state
        .cells_of(session)
        .filter(|(cell, _)| only.is_none_or(|only| only == *cell))
        .map(|(cell, updates)| (cell, updates.iter().cloned().collect()))
        .collect();
    let mut whole = BTreeSet::new();
    let mut shared = Vec::new();
    for (cell, updates) in &held {
        let wanted = match only {
            Some(_) => true,
            None => {
                !shared::finished(updates)? || sim::mutated(sim::Mutation::JournalLoadsFinished)
            }
        };
        if wanted {
            whole.insert(*cell);
            shared.extend(shared::result_keys(session, updates)?);
        }
    }
    restored(state, session, &whole, &shared);
    let mut journals = Vec::new();
    let mut catalogs = HashMap::new();
    for (cell, updates) in &held {
        let loaded = whole.contains(cell);
        let decoded = decoded(state, session, loaded, updates, &mut catalogs)?;
        if let Some(mut journal) = CellJournal::fold(decoded) {
            journal.unloaded = !loaded;
            journals.push(journal);
        }
    }
    Ok(journals)
}

impl CellJournalSink for ThreadJournals {
    fn save(&self, update: CellJournalUpdate) -> HostFuture<'static, Result<(), String>> {
        let inner = Arc::clone(&self.0);
        Box::pin(async move { inner.save(update).await.map_err(|error| error.to_string()) })
    }

    fn forget(&self, session_id: &str, cell_id: u64) -> HostFuture<'static, ()> {
        let inner = Arc::clone(&self.0);
        let cell = (session_id.to_owned(), cell_id);
        inner.unsaved.update(|unsaved| unsaved.remove(&cell));
        inner.answered.update(|owed| owed.remove(&cell));
        inner
            .dispatched
            .update(|dispatched| dispatched.remove(&cell));
        let unfinished = inner.live.update(|live| live.remove(&cell));
        Box::pin(async move {
            let (session, cell) = cell;
            let forgotten = inner
                .log
                .job_staged(move |staged| {
                    staged.push(Record::Forget {
                        cell,
                        session: session.clone(),
                    })
                })
                .await;
            if let Err(error) = forgotten {
                nanocode_trace::warn!(%error, "thread log could not forget a cell journal");
                return;
            }
            if unfinished && let Err(error) = inner.log.flush(None).await {
                nanocode_trace::warn!(%error, "thread log could not commit a forgotten cell journal");
            }
        })
    }

    fn load(&self, session_id: &str) -> HostFuture<'static, Result<Vec<CellJournal>, String>> {
        let journals = self.clone();
        let session = session_id.to_owned();
        Box::pin(async move {
            journals
                .read(move |state| cells(state, &session, None))
                .await
                .map_err(|error| error.to_string())
        })
    }

    fn load_cell(
        &self,
        session_id: &str,
        cell_id: u64,
    ) -> HostFuture<'static, Result<Option<CellJournal>, String>> {
        let journals = self.clone();
        let session = session_id.to_owned();
        Box::pin(async move {
            journals
                .read(move |state| cells(state, &session, Some(cell_id)))
                .await
                .map(|journals| {
                    journals
                        .into_iter()
                        .find(|journal| journal.cell_id == cell_id)
                })
                .map_err(|error| error.to_string())
        })
    }

    fn save_stored(
        &self,
        session_id: &str,
        stored: String,
    ) -> HostFuture<'static, Result<(), String>> {
        let log = Arc::clone(&self.0.log);
        let key = stored_key(session_id, STORED_MARK);
        Box::pin(async move {
            log.job_staged(move |staged| {
                staged.push(Record::Stored {
                    key: key.clone(),
                    value: stored.clone(),
                })
            })
            .await
            .map_err(|error| refused(error).to_string())
        })
    }

    fn load_stored(&self, session_id: &str) -> HostFuture<'static, Result<Option<String>, String>> {
        let journals = self.clone();
        let key = stored_key(session_id, STORED_MARK);
        let legacy = stored_key(STORED_MARK, session_id);
        Box::pin(async move {
            journals
                .read(move |state| {
                    Ok(state
                        .stored(&key)
                        .or_else(|| state.stored(&legacy))
                        .map(str::to_owned))
                })
                .await
                .map_err(|error| error.to_string())
        })
    }
}
