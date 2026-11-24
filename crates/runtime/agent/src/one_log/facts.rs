use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use nanocode_tool_host::ToolOutput;
use nanocode_turnlog::{Fold, Lease, Point};
use serde_json::value::RawValue;

use super::OneLog;
use crate::durability::{
    StoreError, StoreFuture,
    facts::{
        Append, Digest, EffectResult, Fact, FactLog, FactLogs, FactSink, FoldError, Marker,
        OwnedFact, Replay, StoredFact, Value,
    },
};

pub mod sim {
    use std::sync::atomic::{AtomicU8, Ordering};

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    #[repr(u8)]
    pub enum Mutation {
        None,
        CommitBuffered,
        UnseenSkipped,
        IntentReplayed,
        IntentCopied,
        ScopeLeaks,
    }

    impl Mutation {
        pub const ALL: [Self; 5] = [
            Self::CommitBuffered,
            Self::UnseenSkipped,
            Self::IntentReplayed,
            Self::IntentCopied,
            Self::ScopeLeaks,
        ];

        #[must_use]
        pub const fn name(self) -> &'static str {
            match self {
                Self::None => "none",
                Self::CommitBuffered => "commit_buffered",
                Self::UnseenSkipped => "unseen_skipped",
                Self::IntentReplayed => "intent_replayed",
                Self::IntentCopied => "intent_copied",
                Self::ScopeLeaks => "scope_leaks",
            }
        }

        #[must_use]
        pub fn parse(name: &str) -> Option<Self> {
            std::iter::once(Self::None)
                .chain(Self::ALL)
                .find(|mutation| mutation.name() == name)
        }
    }

    static MUTATION: AtomicU8 = AtomicU8::new(Mutation::None as u8);

    pub fn mutate(mutation: Mutation) {
        MUTATION.store(mutation as u8, Ordering::SeqCst);
    }

    pub fn mutated(mutation: Mutation) -> bool {
        MUTATION.load(Ordering::Relaxed) == mutation as u8
    }
}

const fn point_of(fact: &StoredFact) -> Point {
    match fact {
        Fact::Input(_) => Point::Admission,
        Fact::Intent(_) => Point::Call,
        Fact::Marker(Marker::Status { .. }) => Point::Finish,
        Fact::Base(_)
        | Fact::ModelOut(_)
        | Fact::EffectResult(_)
        | Fact::Decision(_)
        | Fact::Marker(_) => Point::Progress,
    }
}

fn scoped(fold: &Fold, scope: &Scope, after: u64) -> (Vec<(u64, StoredFact)>, Vec<Digest>) {
    if after > 0 && sim::mutated(sim::Mutation::UnseenSkipped) {
        return (Vec::new(), Vec::new());
    }
    let facts = if sim::mutated(sim::Mutation::ScopeLeaks) {
        fold.facts_after(after)
            .map(|(seq, fact)| (seq, fact.clone()))
            .collect::<Vec<_>>()
    } else {
        fold.facts_of(&scope.state, scope.turns, after)
            .map(|(seq, fact)| (seq, fact.clone()))
            .collect::<Vec<_>>()
    };
    let mut wanted = BTreeSet::new();
    for (_, fact) in &facts {
        let named: Result<_, std::convert::Infallible> = fact.try_map(
            |value| {
                if let Value::Blob(blob) = value
                    && let Some(digest) = fold.values().digest(*blob)
                {
                    wanted.insert(*digest);
                }
                Ok(())
            },
            |_| Ok(()),
        );
        let Ok(_) = named;
    }
    (facts, wanted.into_iter().collect())
}

pub(super) fn intended(fold: &Fold, call_id: &str) -> Option<String> {
    if sim::mutated(sim::Mutation::IntentCopied) {
        return None;
    }
    fold.intent(call_id).map(|call| call.tool.clone())
}

struct Scope {
    state: String,
    turns: bool,
}

struct Scoped {
    log: Arc<OneLog>,
    scope: Scope,
}

struct Observed<'l, 'a> {
    lease: &'l mut Lease<'a>,
    state: &'l str,
    point: Point,
}

impl FactSink for Observed<'_, '_> {
    fn next(&self) -> u64 {
        self.lease.next()
    }

    fn put_blob(&mut self, digest: &Digest, json: &RawValue) -> Result<(), FoldError> {
        self.lease.put_blob(digest, json)
    }

    fn append(&mut self, fact: StoredFact) -> Result<u64, FoldError> {
        self.point = point_of(&fact);
        let state = match fact {
            Fact::Marker(Marker::Blob { .. }) => None,
            _ => Some(self.state),
        };
        self.lease.append_in(state, fact)
    }
}

impl OneLog {
    async fn replayed(&self, scope: &Scope) -> Result<Replay, StoreError> {
        let (facts, digests) = self.log.read(|fold| scoped(fold, scope, 0)).await;
        let keys = digests
            .iter()
            .map(|digest| digest.key().as_str().to_owned())
            .collect::<Vec<_>>();
        let blobs = self
            .log
            .read_owners(&keys, |fold| {
                digests
                    .iter()
                    .map(|digest| Ok((*digest, fold.blob(digest)?.to_owned())))
                    .collect::<Result<BTreeMap<_, _>, FoldError>>()
            })
            .await??;
        Ok(Replay {
            snapshot: None,
            facts,
            blobs,
        })
    }

    async fn appended(
        &self,
        scope: &Scope,
        after: u64,
        write: Append<'_>,
    ) -> Result<u64, StoreError> {
        let mut lease = self.log.lease_within().await?;
        lease.secure().await?;
        let (facts, digests) = scoped(lease.fold(), scope, after);
        let keys = digests
            .iter()
            .map(|digest| digest.key().as_str().to_owned())
            .collect::<Vec<_>>();
        lease.load_owners(&keys).await?;
        let blobs = digests
            .iter()
            .map(|digest| Ok((*digest, lease.fold().blob(digest)?.to_owned())))
            .collect::<Result<BTreeMap<_, _>, FoldError>>()?;
        let unseen = Replay {
            snapshot: None,
            facts,
            blobs,
        };
        let mut sink = Observed {
            lease: &mut lease,
            state: &scope.state,
            point: Point::Progress,
        };
        let seq = write(unseen, &mut sink)?;
        let point = sink.point;
        if !sim::mutated(sim::Mutation::CommitBuffered) {
            lease.commit(point).await?;
        }
        Ok(seq)
    }
}

impl FactLog for Scoped {
    fn replay(&self) -> StoreFuture<'_, Result<Replay, StoreError>> {
        Box::pin(self.log.replayed(&self.scope))
    }

    fn append<'a>(
        &'a self,
        after: u64,
        write: Append<'a>,
    ) -> StoreFuture<'a, Result<u64, StoreError>> {
        Box::pin(self.log.appended(&self.scope, after, write))
    }
}

pub(super) struct Logs(pub(super) Arc<OneLog>);

impl FactLogs for Logs {
    fn scoped(&self, state: &str, turns: bool) -> Arc<dyn FactLog> {
        Arc::new(Scoped {
            log: Arc::clone(&self.0),
            scope: Scope {
                state: state.to_owned(),
                turns,
            },
        })
    }
}

const UNKNOWN_OUTCOME: &str = "Tool execution interrupted by a host crash; outcome unknown. Do not assume it did not run or automatically repeat it.";

fn unknown_outcome() -> Result<Box<RawValue>, StoreError> {
    let wire = ToolOutput::error(UNKNOWN_OUTCOME)
        .into_wire()
        .map_err(|error| StoreError::NotCommitted(format!("encode an unknown outcome: {error}")))?;
    serde_json::value::to_raw_value(&wire)
        .map_err(|error| StoreError::NotCommitted(format!("encode an unknown outcome: {error}")))
}

impl OneLog {
    pub async fn settle_intents(&self, turn: &str) -> Result<Vec<String>, StoreError> {
        let mut lease = self.log.lease_within().await?;
        lease.secure().await?;
        let dangling = lease
            .fold()
            .intents()
            .filter(|(_, _, call)| call.turn.as_deref().is_none_or(|owner| owner == turn))
            .map(|(call_id, state, _)| (call_id.to_owned(), state.map(str::to_owned)))
            .collect::<Vec<_>>();
        if dangling.is_empty() || sim::mutated(sim::Mutation::IntentReplayed) {
            return Ok(Vec::new());
        }
        let output = unknown_outcome()?;
        for (call_id, state) in &dangling {
            let result: OwnedFact<Box<RawValue>> = Fact::EffectResult(EffectResult {
                call_id: call_id.clone(),
                output: output.clone(),
            });
            lease.append_owned(state.as_deref(), &result)?;
        }
        lease.commit(Point::Recovery).await?;
        Ok(dangling.into_iter().map(|(call_id, _)| call_id).collect())
    }
}
