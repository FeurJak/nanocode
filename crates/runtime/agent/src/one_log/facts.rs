use std::collections::{BTreeMap, BTreeSet};

use nanocode_turnlog::{Fold, Lease, Point};
use serde_json::value::RawValue;

use super::OneLog;
use crate::durability::{
    StoreError, StoreFuture,
    facts::{
        Append, Digest, Fact, FactLog, FactSink, FoldError, Marker, Replay, StoredFact, Value,
        referenced,
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
    }

    impl Mutation {
        pub const ALL: [Self; 2] = [Self::CommitBuffered, Self::UnseenSkipped];

        #[must_use]
        pub const fn name(self) -> &'static str {
            match self {
                Self::None => "none",
                Self::CommitBuffered => "commit_buffered",
                Self::UnseenSkipped => "unseen_skipped",
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
