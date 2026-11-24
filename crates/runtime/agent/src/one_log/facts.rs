use std::collections::{BTreeMap, BTreeSet};

use nanocode_tool_host::ToolOutput;
use nanocode_turnlog::{Fold, Lease, Point};
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

use super::OneLog;
use crate::durability::{
    StoreError, StoreFuture,
    facts::{
        Append, Digest, DurableFold, EffectResult, Fact, FactLog, FactSink, FoldError, Marker,
        Replay, Seen, StoredFact, Value, commit_fact, fold_facts, referenced,
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
    }

    impl Mutation {
        pub const ALL: [Self; 4] = [
            Self::CommitBuffered,
            Self::UnseenSkipped,
            Self::IntentReplayed,
            Self::IntentCopied,
        ];

        #[must_use]
        pub const fn name(self) -> &'static str {
            match self {
                Self::None => "none",
                Self::CommitBuffered => "commit_buffered",
                Self::UnseenSkipped => "unseen_skipped",
                Self::IntentReplayed => "intent_replayed",
                Self::IntentCopied => "intent_copied",
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

fn unseen(fold: &Fold, after: u64) -> (Vec<(u64, StoredFact)>, Vec<Digest>) {
    let facts = fold
        .facts()
        .filter(|(seq, _)| *seq > after && !sim::mutated(sim::Mutation::UnseenSkipped))
        .map(|(seq, fact)| (seq, fact.clone()))
        .collect::<Vec<_>>();
    let mut wanted = BTreeSet::new();
    for (_, fact) in &facts {
        let named: Result<_, std::convert::Infallible> = fact.try_map(
            |value| {
                if let Value::Blob(blob) = value {
                    wanted.insert(blob.index());
                }
                Ok(())
            },
            |_| Ok(()),
        );
        let Ok(_) = named;
    }
    let digests = fold
        .facts()
        .filter_map(|(_, fact)| match fact {
            Fact::Marker(Marker::Blob { digest, .. }) => Some(*digest),
            _ => None,
        })
        .zip(0..)
        .filter(|(_, index)| wanted.contains(index))
        .map(|(digest, _)| digest)
        .collect();
    (facts, digests)
}

pub(super) fn intended(fold: &Fold, call_id: &str) -> Option<String> {
    if sim::mutated(sim::Mutation::IntentCopied) {
        return None;
    }
    fold.facts().find_map(|(_, fact)| match fact {
        Fact::Intent(intent) if intent.call_id == call_id => Some(intent.tool.clone()),
        _ => None,
    })
}

struct Observed<'l, 'a> {
    lease: &'l mut Lease<'a>,
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
        self.lease.append(fact)
    }
}

impl OneLog {
    async fn replayed(&self) -> Result<Replay, StoreError> {
        let (snapshot, facts) = self
            .log
            .read(|fold| {
                let facts = fold
                    .facts()
                    .map(|(seq, fact)| (seq, fact.clone()))
                    .collect::<Vec<_>>();
                fold.cached().map(|snapshot| (snapshot, facts))
            })
            .await?;
        let digests = referenced(facts.iter().map(|(_, fact)| fact));
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
            snapshot,
            facts,
            blobs,
        })
    }

    async fn appended(&self, after: u64, write: Append<'_>) -> Result<u64, StoreError> {
        let mut lease = self.log.lease_within().await?;
        lease.secure().await?;
        let (facts, digests) = unseen(lease.fold(), after);
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

impl FactLog for OneLog {
    fn replay(&self) -> StoreFuture<'_, Result<Replay, StoreError>> {
        Box::pin(self.replayed())
    }

    fn append<'a>(
        &'a self,
        after: u64,
        write: Append<'a>,
    ) -> StoreFuture<'a, Result<u64, StoreError>> {
        Box::pin(self.appended(after, write))
    }
}

const UNKNOWN_OUTCOME: &str = "Tool execution interrupted by a host crash; outcome unknown. Do not assume it did not run or automatically repeat it.";

#[derive(Default, Deserialize, Serialize)]
struct Calls {
    open: BTreeMap<String, Option<String>>,
}

impl DurableFold for Calls {
    const FAMILY: &'static str = "nanocode.calls";
    const FORMAT: u32 = 1;
    type Decision = Box<RawValue>;

    fn apply(&mut self, seq: u64, fact: Seen<'_, Box<RawValue>>) -> Result<(), FoldError> {
        match fact {
            Fact::Intent(intent) => {
                if self.open.insert(intent.call_id, intent.turn).is_some() {
                    return Err(FoldError::Rejected {
                        seq,
                        reason: "an intent repeats a call that has no result yet".to_owned(),
                    });
                }
            }
            Fact::EffectResult(result) => {
                self.open.remove(&result.call_id);
            }
            Fact::Base(_)
            | Fact::Input(_)
            | Fact::ModelOut(_)
            | Fact::Decision(_)
            | Fact::Marker(_) => {}
        }
        Ok(())
    }

    fn request(&self) -> Result<Vec<u8>, FoldError> {
        serde_json::to_vec(&self.open).map_err(|error| FoldError::Encode(error.to_string()))
    }
}

fn unknown_outcome() -> Result<Box<RawValue>, StoreError> {
    let wire = ToolOutput::error(UNKNOWN_OUTCOME)
        .into_wire()
        .map_err(|error| StoreError::NotCommitted(format!("encode an unknown outcome: {error}")))?;
    serde_json::value::to_raw_value(&wire)
        .map_err(|error| StoreError::NotCommitted(format!("encode an unknown outcome: {error}")))
}

impl OneLog {
    pub async fn settle_intents(&self, turn: &str) -> Result<Vec<String>, StoreError> {
        let mut calls = fold_facts::<Calls>(self).await?;
        let dangling = calls
            .state()
            .open
            .iter()
            .filter(|(_, owner)| owner.as_deref().is_none_or(|owner| owner == turn))
            .map(|(call_id, _)| call_id.clone())
            .collect::<Vec<_>>();
        if sim::mutated(sim::Mutation::IntentReplayed) {
            return Ok(Vec::new());
        }
        for call_id in &dangling {
            let result = Fact::EffectResult(EffectResult {
                call_id: call_id.clone(),
                output: unknown_outcome()?,
            });
            calls = commit_fact(self, calls, result).await?.0;
        }
        Ok(dangling)
    }
}
