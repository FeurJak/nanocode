use std::{collections::BTreeMap, sync::Arc};

use nanocode_scope::ScopeError;
use nanocode_turnlog::{CachedAt, Record, cache_key};
use serde_json::value::RawValue;

use super::{Staged, ThreadLog, ThreadState, sim};
use crate::{
    durability::{
        StoreError, StoreFuture,
        facts::{
            Append, Digest, Fact, FactLog, FactLogs, FactSink, FoldError, Marker, Replay, Snapshot,
            StoredFact, Value,
        },
    },
    one_log::{FactScope, in_scope},
};

#[derive(Clone)]
pub struct ThreadFacts {
    log: Arc<ThreadLog>,
}

impl ThreadFacts {
    #[must_use]
    pub const fn new(log: Arc<ThreadLog>) -> Self {
        Self { log }
    }
}

impl FactLogs for ThreadFacts {
    fn scoped(&self, state: &str, turns: bool) -> Arc<dyn FactLog> {
        Arc::new(Scoped {
            log: Arc::clone(&self.log),
            scope: FactScope {
                state: state.to_owned(),
                turns,
            },
        })
    }
}

struct Scoped {
    log: Arc<ThreadLog>,
    scope: FactScope,
}

fn refused(error: ScopeError) -> StoreError {
    match error {
        ScopeError::Store(error) => error,
        ScopeError::Busy => StoreError::Busy,
        ScopeError::Lost => StoreError::Fenced,
        error => StoreError::NotCommitted(error.to_string()),
    }
}

fn wanted(
    state: &ThreadState,
    facts: &[(u64, StoredFact)],
) -> Result<BTreeMap<Digest, Box<RawValue>>, StoreError> {
    let mut blobs = BTreeMap::new();
    for (seq, fact) in facts {
        let named: Result<_, StoreError> = fact.try_map(
            |value| {
                let Value::Blob(blob) = value else {
                    return Ok(());
                };
                let digest = state.blob_digest(*blob).ok_or_else(|| {
                    StoreError::Backend(format!(
                        "fact {seq} names blob {} that no marker before it holds",
                        blob.index()
                    ))
                })?;
                let body = state.blob_body(digest).ok_or_else(|| {
                    StoreError::Backend(format!("fact {seq} names a blob whose body is missing"))
                })?;
                let json = RawValue::from_string(body.to_owned()).map_err(|error| {
                    StoreError::Backend(format!("the blob fact {seq} names is not JSON: {error}"))
                })?;
                blobs.insert(*digest, json);
                Ok(())
            },
            |_| Ok(()),
        );
        named?;
    }
    Ok(blobs)
}

fn replayed(
    state: &ThreadState,
    scope: &FactScope,
    after: u64,
    snapshot: Option<Snapshot>,
) -> Result<Replay, StoreError> {
    let facts: Vec<(u64, StoredFact)> = state
        .facts_after(after)
        .filter(|(_, kept)| in_scope(scope, kept.state.as_deref(), &kept.fact))
        .map(|(seq, kept)| (seq, kept.fact.clone()))
        .collect();
    let blobs = wanted(state, &facts)?;
    Ok(Replay {
        snapshot,
        facts,
        blobs,
    })
}

fn cached(state: &ThreadState, scope: &FactScope) -> Result<Option<Snapshot>, StoreError> {
    let Some(value) = state.cached_state(&scope.state) else {
        return Ok(None);
    };
    let body = match serde_json::from_str::<CachedAt>(value.get()) {
        Ok(at) => state
            .blob_body(&at.blob)
            .ok_or_else(|| {
                StoreError::Backend(format!(
                    "the cached fold of {} names a blob whose body is missing",
                    scope.state
                ))
            })?
            .to_owned(),
        Err(_) => value.get().to_owned(),
    };
    serde_json::from_str(&body).map(Some).map_err(|error| {
        StoreError::Backend(format!(
            "the cached fold of {} does not decode: {error}",
            scope.state
        ))
    })
}

impl Scoped {
    fn state(&self) -> Result<Arc<ThreadState>, StoreError> {
        self.log.state().ok_or_else(|| {
            refused(ScopeError::Routed {
                holder: self.log.holder().unwrap_or_default().to_owned(),
            })
        })
    }
}

impl FactLog for Scoped {
    fn replay(&self) -> StoreFuture<'_, Result<Replay, StoreError>> {
        Box::pin(async move { replayed(&*self.state()?, &self.scope, 0, None) })
    }

    fn replay_cached<'a>(
        &'a self,
        family: &'a str,
        format: u32,
    ) -> StoreFuture<'a, Result<Replay, StoreError>> {
        Box::pin(async move {
            let state = self.state()?;
            let snapshot = cached(&state, &self.scope)?
                .filter(|snapshot| snapshot.family == family && snapshot.format == format);
            let after = snapshot.as_ref().map_or(0, |snapshot| snapshot.seq);
            replayed(&state, &self.scope, after, snapshot)
        })
    }

    fn append(&self, after: u64, write: Append) -> StoreFuture<'_, Result<u64, StoreError>> {
        let scope = self.scope.clone();
        Box::pin(async move {
            self.log
                .job_once(move |staged| {
                    let mut unseen = replayed(staged.state(), &scope, after, None)?;
                    if after > 0 && sim::mutated(sim::Mutation::FactsUnseenSkipped) {
                        unseen.facts.clear();
                        unseen.blobs.clear();
                    }
                    let mut sink = Sink {
                        staged,
                        state: &scope.state,
                    };
                    write(unseen, &mut sink).map_err(StoreError::from)
                })
                .await
                .map_err(refused)
        })
    }
}

struct Sink<'a> {
    staged: &'a mut Staged,
    state: &'a str,
}

impl Sink<'_> {
    fn put(&mut self, record: Record) -> Result<(), FoldError> {
        self.staged.push(record).map_err(FoldError::Sink)
    }
}

impl FactSink for Sink<'_> {
    fn next(&self) -> u64 {
        self.staged.state().next_fact()
    }

    fn put_blob(&mut self, digest: &Digest, json: &RawValue) -> Result<(), FoldError> {
        assert_eq!(
            Digest::of(json.get().as_bytes()),
            *digest,
            "a blob is put under the digest of its value"
        );
        if self.staged.state().blob_body(digest).is_some() {
            return Ok(());
        }
        self.put(Record::Stored {
            key: digest.key().as_str().to_owned(),
            value: json.get().to_owned(),
        })
    }

    fn append(&mut self, fact: StoredFact) -> Result<u64, FoldError> {
        let state = match fact {
            Fact::Marker(Marker::Blob { .. }) => None,
            _ => Some(self.state.to_owned()),
        };
        let seq = self.next();
        self.put(Record::Fact { fact, seq, state })?;
        Ok(seq)
    }

    fn cache_due(&self) -> bool {
        self.staged.state().cache_due(self.state)
    }

    fn cache(&mut self, snapshot: &Snapshot) -> Result<Vec<(u64, StoredFact)>, FoldError> {
        assert!(
            snapshot.seq < self.next(),
            "a cached fold names a fact the thread holds"
        );
        let state = self.state;
        let encode = move |error: serde_json::Error| {
            FoldError::Sink(StoreError::NotCommitted(format!(
                "encode the cached fold of {state}: {error}"
            )))
        };
        let value = serde_json::value::to_raw_value(snapshot).map_err(encode)?;
        let blob = Digest::of(value.get().as_bytes());
        let bytes = value.get().len() as u64;
        let mut appended = Vec::new();
        if self.staged.state().blob_body(&blob).is_none() {
            self.put_blob(&blob, &value)?;
            let marker: StoredFact = Fact::Marker(Marker::Blob {
                digest: blob,
                bytes,
            });
            appended.push((self.append(marker.clone())?, marker));
        }
        let at = serde_json::value::to_raw_value(&CachedAt { blob, bytes }).map_err(encode)?;
        self.put(Record::Host {
            key: cache_key(self.state),
            value: at,
        })?;
        Ok(appended)
    }
}
