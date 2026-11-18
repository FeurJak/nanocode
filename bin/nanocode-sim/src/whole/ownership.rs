use std::sync::Arc;

use nanocode_agent_core::{HarnessFamily, ModelSelection};
use nanocode_durability::{
    Admission, DurableOwner, DurableSession, Error, MemoryStore, SessionRecord, SessionStore,
};
use nanocode_durability_core::{
    Committed, EncodedPayload, OwnedState, OwnerId, OwnerToken, Settle, StateCommit, StateStore,
    StoreError, StoreFuture, StoreRecord, StoredState,
};
use nanocode_sync::Atom;
use serde_json::{Value, json};

#[derive(Clone, Default)]
struct Trace {
    acquired: bool,
    reject: bool,
    writes: Vec<Settle>,
}

struct Observed {
    store: MemoryStore,
    trace: Arc<Atom<Trace>>,
}

impl StateStore for Observed {
    fn read_record<'a>(
        &'a mut self,
        id: &'a str,
        key: &'a str,
    ) -> StoreFuture<'a, Result<Option<String>, StoreError>> {
        self.store.read_record(id, key)
    }

    fn peek<'a>(&'a mut self, id: &'a str) -> StoreFuture<'a, Result<StoredState, StoreError>> {
        self.store.peek(id)
    }

    fn owner<'a>(
        &'a mut self,
        id: &'a str,
    ) -> StoreFuture<'a, Result<Option<OwnerToken>, StoreError>> {
        self.store.owner(id)
    }

    fn acquire<'a>(
        &'a mut self,
        id: &'a str,
        owner: OwnerId,
    ) -> StoreFuture<'a, Result<OwnedState, StoreError>> {
        Box::pin(async move {
            let acquired = self.store.acquire(id, owner).await?;
            self.trace.update(|trace| trace.acquired = true);
            Ok(acquired)
        })
    }

    fn replace<'a>(
        &'a mut self,
        id: &'a str,
        owner: &'a OwnerToken,
        revision: u64,
        payload: &'a str,
        records: &'a [StoreRecord],
    ) -> StoreFuture<'a, Result<u64, StoreError>> {
        Box::pin(async move {
            assert!(
                self.trace.read(|trace| trace.acquired),
                "replace preceded acquisition"
            );
            self.trace
                .update(|trace| trace.writes.push(Settle::Durable));
            self.store
                .replace(id, owner, revision, payload, records)
                .await
        })
    }

    fn commit<'a>(
        &'a mut self,
        id: &'a str,
        owner: &'a OwnerToken,
        revision: u64,
        write: StateCommit<'a>,
    ) -> StoreFuture<'a, Result<Committed, StoreError>> {
        Box::pin(async move {
            assert!(
                self.trace.read(|trace| trace.acquired),
                "commit preceded acquisition"
            );
            if self.trace.update(|trace| std::mem::take(&mut trace.reject)) {
                return Err(StoreError::NotCommitted(
                    "injected admission failure".to_owned(),
                ));
            }
            self.trace.update(|trace| trace.writes.push(write.settle));
            self.store.commit(id, owner, revision, write).await
        })
    }
}

enum Client<'a> {
    Direct(&'a DurableSession),
    Agent(&'a DurableOwner),
}

impl Client<'_> {
    async fn admit(
        &self,
        id: &str,
        input: &Value,
        automatic: bool,
    ) -> Result<(String, Admission<Value, Value>), Error> {
        match (self, automatic) {
            (Self::Direct(session), false) => {
                Ok((id.to_owned(), session.admit_typed(id, input).await?))
            }
            (Self::Agent(owner), false) => Ok((
                id.to_owned(),
                owner.admit_typed(id.to_owned(), input).await?,
            )),
            (Self::Direct(session), true) => {
                Ok(session.admit_automatic_typed(id, input).await?.into_parts())
            }
            (Self::Agent(owner), true) => Ok(owner
                .admit_automatic_typed(id.to_owned(), input)
                .await?
                .into_parts()),
        }
    }

    async fn complete(&self, id: &str) -> Result<(), Error> {
        match self {
            Self::Direct(session) => {
                session.begin_attempt(id).await?;
                session.complete(id, &json!({}), &json!("done")).await
            }
            Self::Agent(owner) => {
                owner.begin_attempt(id.to_owned()).await?;
                owner
                    .complete(
                        id.to_owned(),
                        EncodedPayload::encode(&json!({}))?,
                        &json!("done"),
                    )
                    .await
            }
        }
    }

    async fn release(&self, id: &str) -> Result<(), Error> {
        match self {
            Self::Direct(session) => session.release(id).await,
            Self::Agent(owner) => owner.release_claim(id.to_owned()).await,
        }
    }
}

pub(super) async fn check() -> Result<(), Error> {
    for deferred in [false, true] {
        for agent in [false, true] {
            for automatic in [false, true] {
                let trace = Arc::new(Atom::new(Trace::default()));
                let store = SessionStore::new(Observed {
                    store: MemoryStore::new()?,
                    trace: Arc::clone(&trace),
                })?;
                let store = if deferred {
                    store.with_deferred_start()
                } else {
                    store
                };
                let record = SessionRecord::root(
                    "ownership",
                    ModelSelection::new(HarnessFamily::Claude, "claude-sonnet-4-6"),
                    None,
                );
                let session = store.prepare_session(record).await?;
                assert!(
                    !trace.read(|trace| trace.acquired),
                    "preparation acquired an owner"
                );
                let owner = if agent {
                    Some(session.acquire_agent().await?.0)
                } else {
                    None
                };
                let client = match &owner {
                    Some(owner) => Client::Agent(owner),
                    None => Client::Direct(&session),
                };
                let input = json!({"prompt": "first"});
                if agent {
                    let revision = session.state().await?.revision();
                    trace.update(|trace| trace.reject = true);
                    assert!(client.admit("first", &input, automatic).await.is_err());
                    assert_eq!(session.state().await?.revision(), revision);
                    assert!(session.state().await?.operation("first").is_none());
                }
                let (id, first) = client.admit("first", &input, automatic).await?;
                assert_eq!(id, "first");
                assert!(matches!(first, Admission::Accepted));
                assert!(trace.read(|trace| trace.acquired));
                if deferred {
                    assert_eq!(
                        trace.read(|trace| trace.writes.clone()),
                        [Settle::Ordered, Settle::Ordered]
                    );
                }
                let before = session.state().await?.revision();
                let writes = trace.read(|trace| trace.writes.len());
                assert!(matches!(
                    client.admit("first", &input, false).await,
                    Err(Error::OperationActive { .. })
                ));
                assert!(matches!(
                    client
                        .admit("first", &json!({"prompt": "different"}), false)
                        .await,
                    Err(Error::OperationConflict { .. })
                ));
                assert_eq!(session.state().await?.revision(), before);
                assert_eq!(trace.read(|trace| trace.writes.len()), writes);
                client.complete("first").await?;
                assert!(matches!(
                    client.admit("first", &input, false).await?.1,
                    Admission::Completed { .. }
                ));
                let checkpoint = session.state().await?.revision();
                trace.update(|trace| trace.reject = true);
                assert!(
                    client
                        .admit("second", &json!("second"), automatic)
                        .await
                        .is_err()
                );
                assert_eq!(session.state().await?.revision(), checkpoint);
                assert!(session.state().await?.operation("second").is_none());
                assert!(matches!(
                    client.admit("second", &json!("second"), automatic).await?.1,
                    Admission::Accepted
                ));
                assert_eq!(
                    trace.read(|trace| trace.writes.last().copied()),
                    Some(Settle::Durable),
                    "initial settlement obligation survived successful admission"
                );
                client.release("second").await?;
                let (id, replay) = client.admit("candidate", &json!("second"), true).await?;
                assert_eq!(id, "second");
                assert!(matches!(replay, Admission::Pending));
                client.complete("second").await?;
                drop(owner);
                let reopened = session.open_sibling("ownership").await?;
                assert!(matches!(
                    reopened
                        .admit_typed::<_, Value, Value>("first", &input)
                        .await?,
                    Admission::Completed { .. }
                ));
                assert!(matches!(
                    reopened
                        .admit_typed::<_, Value, Value>("second", &json!("second"))
                        .await?,
                    Admission::Completed { .. }
                ));
            }
        }
    }
    Ok(())
}
