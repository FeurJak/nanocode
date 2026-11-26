use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use nanocode_scope::Adopted;
use nanocode_sync::Slot;
use nanocode_turnlog::{
    ArmedWake, Checkpoint, Compaction, Event, Extension, Fold, Hold, LogHost, OpenCall, Phase,
    RETIRED_KEY, Retired, Rider, ScopeRows, StoredCheckpoint, Turn, TurnLog, Write,
};
use serde_json::value::RawValue;

use super::{
    sim::{self, Mutation},
    state::{KeptFact, ThreadState, invalid},
};
use crate::durability::{
    StoreError, StoreFuture,
    facts::{BlobRef, Digest, StoredFact, Value},
};

pub(crate) const RETIRED_DIGEST: &str = "retired:";

struct Counted {
    log: Arc<dyn LogHost>,
    loaded: Slot<Vec<(u64, String)>>,
    rows: AtomicU64,
    bytes: AtomicU64,
}

impl Counted {
    fn count(&self, rows: &[(u64, String)]) {
        let bytes: usize = rows.iter().map(|(_, body)| body.len()).sum();
        self.rows.fetch_add(rows.len() as u64, Ordering::Relaxed);
        self.bytes.fetch_add(bytes as u64, Ordering::Relaxed);
    }
}

fn refused(scope: &str) -> ! {
    panic!("the adoption of {scope} reads the turn log and writes nothing to it")
}

impl LogHost for Counted {
    fn call_bound(&self) -> Option<std::time::Duration> {
        self.log.call_bound()
    }

    fn scan<'a>(
        &'a self,
        scope: &'a str,
        from: u64,
        to: u64,
    ) -> StoreFuture<'a, Result<Vec<(u64, String)>, StoreError>> {
        Box::pin(async move {
            let rows = self.log.scan(scope, from, to).await?;
            self.count(&rows);
            Ok(rows)
        })
    }

    fn scans<'a>(
        &'a self,
        scans: &'a [(&'a str, u64)],
    ) -> StoreFuture<'a, Result<Vec<ScopeRows>, StoreError>> {
        Box::pin(async move {
            let scanned = self.log.scans(scans).await?;
            for rows in &scanned {
                self.count(rows);
            }
            Ok(scanned)
        })
    }

    fn load<'a>(
        &'a self,
        scope: &'a str,
    ) -> StoreFuture<'a, Result<Vec<(u64, String)>, StoreError>> {
        Box::pin(async move {
            if let Some(rows) = self.loaded.take() {
                return Ok(rows);
            }
            let rows = self.log.load(scope).await?;
            self.count(&rows);
            Ok(rows)
        })
    }

    fn append<'a>(
        &'a self,
        scope: &'a str,
        _rows: &'a [(u64, String)],
    ) -> StoreFuture<'a, Result<(), StoreError>> {
        refused(scope)
    }

    fn append_compacting<'a>(
        &'a self,
        scope: &'a str,
        _rows: &'a [(u64, String)],
        _compaction: Compaction<'a>,
    ) -> StoreFuture<'a, Result<(), StoreError>> {
        refused(scope)
    }

    fn extend<'a>(
        &'a self,
        scope: &'a str,
        _extension: Extension<'a>,
    ) -> Option<StoreFuture<'a, Result<(), StoreError>>> {
        refused(scope)
    }

    fn carry<'a>(
        &'a self,
        scope: &'a str,
        _write: Write<'a>,
        _rider: Rider<'a>,
    ) -> Option<StoreFuture<'a, Result<(), StoreError>>> {
        refused(scope)
    }

    fn checkpoint<'a>(
        &'a self,
        scope: &'a str,
    ) -> Option<StoreFuture<'a, Result<Option<StoredCheckpoint>, StoreError>>> {
        let read = self.log.checkpoint(scope)?;
        Some(Box::pin(async move {
            let checkpoint = read.await?;
            if let Some(checkpoint) = &checkpoint {
                self.rows.fetch_add(1, Ordering::Relaxed);
                self.bytes
                    .fetch_add(checkpoint.body.len() as u64, Ordering::Relaxed);
            }
            Ok(checkpoint)
        }))
    }

    fn save_checkpoint<'a>(
        &'a self,
        scope: &'a str,
        _checkpoint: &'a StoredCheckpoint,
    ) -> Option<StoreFuture<'a, Result<(), StoreError>>> {
        refused(scope)
    }
}

fn plain(rows: &[(u64, String)]) -> bool {
    let mut state = ThreadState::default();
    rows.iter().all(|(seq, row)| state.load(*seq, row).is_ok())
}

type Facts = Vec<(u64, Option<String>, StoredFact)>;

struct View {
    turns: Vec<Turn>,
    retired: Vec<(String, Retired)>,
    retired_count: usize,
    digests: Vec<(BlobRef, Option<Digest>)>,
    cells: Vec<(String, u64, Vec<Box<RawValue>>)>,
    checkpoints: Vec<(String, Checkpoint)>,
    stored: Vec<(String, String)>,
    hosts: Vec<(String, Box<RawValue>)>,
    holds: Vec<(String, Hold)>,
    calls: Vec<(String, OpenCall)>,
    models: Vec<(String, Vec<Box<RawValue>>)>,
    wakes: Vec<(String, ArmedWake)>,
    generation: u64,
}

fn owned<'a, V: Clone + 'a>(values: impl Iterator<Item = (&'a str, &'a V)>) -> Vec<(String, V)> {
    values
        .map(|(key, value)| (key.to_owned(), value.clone()))
        .collect()
}

impl View {
    fn of(fold: &Fold, ids: &BTreeSet<String>, blobs: &[BlobRef]) -> Self {
        let retired = ids
            .iter()
            .filter(|id| fold.turn(id).is_none())
            .filter_map(|id| {
                fold.retired(id)
                    .map(|retired| (id.clone(), retired.clone()))
            })
            .collect();
        Self {
            turns: fold.turns().to_vec(),
            retired,
            retired_count: fold.retired_turns(),
            digests: blobs
                .iter()
                .map(|blob| (*blob, fold.values().digest(*blob).copied()))
                .collect(),
            cells: fold
                .cells()
                .map(|(session, cell, updates)| (session.to_owned(), cell, updates.to_vec()))
                .collect(),
            checkpoints: owned(fold.checkpoints()),
            stored: fold
                .stored_all()
                .map(|(key, value)| (key.to_owned(), value.to_owned()))
                .collect(),
            hosts: fold
                .hosts()
                .filter(|(key, _)| *key != RETIRED_KEY)
                .map(|(key, value)| (key.to_owned(), value.to_owned()))
                .collect(),
            holds: owned(fold.holds()),
            calls: owned(fold.open_calls()),
            models: fold
                .models_all()
                .map(|(turn, responses)| (turn.to_owned(), responses.to_vec()))
                .collect(),
            wakes: owned(fold.wakes().iter()),
            generation: fold.wakes().generation(),
        }
    }
}

fn blob_refs(facts: &Facts) -> Vec<BlobRef> {
    let mut refs = BTreeMap::new();
    for (_, _, fact) in facts {
        let seen: Result<_, std::convert::Infallible> = fact.try_map(
            |value| {
                if let Value::Blob(blob) = value {
                    refs.insert(blob.index(), *blob);
                }
                Ok(())
            },
            |_| Ok(()),
        );
        let Ok(_) = seen;
    }
    refs.into_values().collect()
}

fn inlined(
    seq: u64,
    fact: &StoredFact,
    values: &BTreeMap<u32, Box<RawValue>>,
) -> Result<StoredFact, StoreError> {
    if sim::mutated(Mutation::AdoptionSkipsBlobs) {
        return Ok(fact.clone());
    }
    fact.try_map(
        |value| match value {
            Value::Inline(json) => Ok(Value::Inline(json.clone())),
            Value::Blob(blob) => values
                .get(&blob.index())
                .map(|json| Value::Inline(json.clone()))
                .ok_or_else(|| {
                    invalid(format!(
                        "fact {seq} names blob {} that the turn log does not hold",
                        blob.index()
                    ))
                }),
        },
        |decision| Ok(decision.clone()),
    )
}

fn retired_turn(id: String, retired: &Retired, events: &[Event]) -> Result<Turn, StoreError> {
    let result = events
        .binary_search_by_key(&retired.last, |event| event.seq)
        .map(|at| events[at].event.clone())
        .map_err(|_| {
            invalid(format!(
                "retired turn {id} ends at event {}, which the turn log does not hold",
                retired.last
            ))
        })?;
    Ok(Turn {
        digest: format!("{RETIRED_DIGEST}{:032x}", retired.digest),
        edits: BTreeMap::new(),
        first: retired.first,
        hand: retired.hand.as_deref().map(str::to_owned),
        id,
        metadata: RawValue::NULL.to_owned(),
        phase: Phase::Finished {
            last: retired.last,
            outcome: retired.outcome,
            result,
        },
        prompt: RawValue::NULL.to_owned(),
        recovered: None,
    })
}

fn turns(state: &mut ThreadState, view: &mut View, events: &[Event]) -> Result<(), StoreError> {
    if view.retired.len() != view.retired_count {
        return Err(invalid(format!(
            "the turn log retired {} turns and its events name {} of them",
            view.retired_count,
            view.retired.len()
        )));
    }
    let mut turns = std::mem::take(&mut view.turns);
    if !sim::mutated(Mutation::AdoptionSkipsRetired) {
        for (id, retired) in std::mem::take(&mut view.retired) {
            turns.push(retired_turn(id, &retired, events)?);
        }
    }
    for turn in turns {
        let id: Arc<str> = Arc::from(turn.id.as_str());
        if state.admitted.insert(turn.first, Arc::clone(&id)).is_some() {
            return Err(invalid(format!(
                "turn {id} starts at event {}, where another turn starts",
                turn.first
            )));
        }
        if state.turns.insert(id, Arc::new(turn)).is_some() {
            return Err(invalid("the turn log holds one turn twice"));
        }
    }
    Ok(())
}

fn events(state: &mut ThreadState, events: Vec<Event>) -> Result<(), StoreError> {
    for pair in events.windows(2) {
        if pair[1].seq <= pair[0].seq {
            return Err(invalid(format!(
                "the turn log answers event {} after event {}",
                pair[1].seq, pair[0].seq
            )));
        }
    }
    state.events = events.into_iter().map(Arc::new).collect();
    Ok(())
}

fn facts(
    state: &mut ThreadState,
    facts: &Facts,
    values: &BTreeMap<u32, Box<RawValue>>,
) -> Result<(), StoreError> {
    for (seq, owner, fact) in facts {
        if *seq != state.next_fact() {
            return Err(invalid(format!(
                "the turn log answers fact {seq} after fact {}",
                state.next_fact() - 1
            )));
        }
        let fact = inlined(*seq, fact, values)?;
        state.index_blob(&fact);
        state.facts.push_back(Arc::new(KeptFact {
            fact,
            state: owner.clone(),
        }));
    }
    Ok(())
}

fn keep(state: &mut ThreadState, view: View) {
    for (session, cell, updates) in view.cells {
        state.cells.insert(
            (Arc::from(session.as_str()), cell),
            updates.into_iter().collect(),
        );
    }
    for (key, checkpoint) in view.checkpoints {
        state
            .checkpoints
            .insert(Arc::from(key.as_str()), Arc::new(checkpoint));
    }
    for (key, value) in view.stored {
        state
            .stored
            .insert(Arc::from(key.as_str()), Arc::from(value.as_str()));
    }
    for (key, value) in view.hosts {
        state.hosts.insert(Arc::from(key.as_str()), value);
    }
    for (key, hold) in view.holds {
        state.holds.insert(Arc::from(key.as_str()), Arc::new(hold));
    }
    for (id, call) in view.calls {
        state.calls.insert(Arc::from(id.as_str()), Arc::new(call));
    }
    for (turn, responses) in view.models {
        state
            .models
            .insert(Arc::from(turn.as_str()), responses.into_iter().collect());
    }
    for (key, wake) in view.wakes {
        state.wakes.insert(Arc::from(key.as_str()), Arc::new(wake));
    }
    state.generation = view.generation;
}

async fn values(
    legacy: &TurnLog,
    digests: &[(BlobRef, Option<Digest>)],
) -> Result<BTreeMap<u32, Box<RawValue>>, StoreError> {
    let mut wanted = Vec::with_capacity(digests.len());
    for (blob, digest) in digests {
        let Some(digest) = digest else {
            return Err(invalid(format!(
                "blob {} has no digest in the turn log",
                blob.index()
            )));
        };
        wanted.push(*digest);
    }
    let resolved = legacy.blobs(&wanted).await?;
    let mut values = BTreeMap::new();
    for ((blob, _), digest) in digests.iter().zip(&wanted) {
        let Some(json) = resolved.get(digest) else {
            return Err(invalid(format!(
                "blob {} is missing from the turn log",
                blob.index()
            )));
        };
        values.insert(blob.index(), json.clone());
    }
    Ok(values)
}

pub(super) async fn adopt(
    log: &Arc<dyn LogHost>,
    scope: &str,
    part_rows: usize,
) -> Result<Option<Adopted<ThreadState>>, StoreError> {
    let counted = Arc::new(Counted {
        log: Arc::clone(log),
        loaded: Slot::new(None),
        rows: AtomicU64::new(0),
        bytes: AtomicU64::new(0),
    });
    let loaded = log.load(scope).await?;
    counted.count(&loaded);
    if loaded.is_empty() || plain(&loaded) {
        return Ok(None);
    }
    counted.loaded.put(loaded);
    let legacy = TurnLog::open(Arc::clone(&counted) as Arc<dyn LogHost>, scope).await?;
    let next = legacy.load_all().await?;
    let history = legacy
        .history(0, Vec::new(), |events: &mut Vec<Event>, event| {
            events.push(event.clone());
            true
        })
        .await?;
    let read = legacy
        .facts_from(0, Vec::new(), |facts: &mut Facts, seq, owner, fact| {
            facts.push((seq, owner.map(str::to_owned), fact.clone()));
        })
        .await?;
    let refs = blob_refs(&read);
    let ids: BTreeSet<String> = history
        .iter()
        .filter_map(|event| event.turn.clone())
        .collect();
    let mut view = legacy.read(move |fold| View::of(fold, &ids, &refs)).await;
    let values = values(&legacy, &view.digests).await?;
    let mut state = ThreadState::default();
    turns(&mut state, &mut view, &history)?;
    events(&mut state, history)?;
    facts(&mut state, &read, &values)?;
    keep(&mut state, view);
    state.bytes = super::parts::encode(&state, part_rows)
        .iter()
        .map(|part| part.body.len())
        .sum();
    Ok(Some(Adopted {
        state,
        next,
        rows: counted.rows.load(Ordering::Relaxed),
        bytes: counted.bytes.load(Ordering::Relaxed),
    }))
}
