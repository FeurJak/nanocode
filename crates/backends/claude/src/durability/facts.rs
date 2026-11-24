use std::sync::Arc;

use nanocode_agent_core::{NanocodeError, Result};
use nanocode_durability::facts::{FactLog, Folded, OwnedFact, commit_fact, fold_facts};

use super::fold::ClaudeFold;

#[doc(hidden)]
pub struct ClaudeFacts {
    log: Arc<dyn FactLog>,
    folded: nanocode_sync::Slot<Folded<ClaudeFold>>,
    synced: nanocode_sync::Slot<Vec<u64>>,
    order: nanocode_sync::Sequencer,
}

fn store_error(error: nanocode_durability::StoreError) -> NanocodeError {
    nanocode_durability::agent_store_error(error)
}

impl ClaudeFacts {
    pub(crate) async fn open(log: Arc<dyn FactLog>) -> Result<Self> {
        let folded = fold_facts::<ClaudeFold>(&*log).await.map_err(store_error)?;
        Ok(Self {
            log,
            folded: nanocode_sync::Slot::new(Some(folded)),
            synced: nanocode_sync::Slot::default(),
            order: nanocode_sync::Sequencer::new(),
        })
    }

    pub(crate) async fn read<R>(&self, read: impl FnOnce(&ClaudeFold) -> R) -> Result<R> {
        let _turn = self.order.enter().await;
        let folded = self.taken()?;
        let value = read(folded.state());
        self.folded.put(folded);
        Ok(value)
    }

    pub(crate) async fn seq(&self) -> Result<u64> {
        let _turn = self.order.enter().await;
        let folded = self.taken()?;
        let seq = folded.seq();
        self.folded.put(folded);
        Ok(seq)
    }

    fn taken(&self) -> Result<Folded<ClaudeFold>> {
        self.folded.take().ok_or_else(|| {
            NanocodeError::InvalidRequest(
                "the Claude fact log could not be refolded after a failed commit".into(),
            )
        })
    }

    pub(crate) async fn commit(&self, fact: OwnedFact<super::ClaudeDecision>) -> Result<u64> {
        let _turn = self.order.enter().await;
        let folded = self.taken()?;
        match commit_fact(&*self.log, folded, fact).await {
            Ok((folded, seq)) => {
                self.folded.put(folded);
                Ok(seq)
            }
            Err(error) => {
                self.synced.take();
                if let Ok(folded) = fold_facts::<ClaudeFold>(&*self.log).await {
                    self.folded.put(folded);
                }
                Err(store_error(error))
            }
        }
    }

    pub(crate) async fn refresh(&self) -> Result<()> {
        let _turn = self.order.enter().await;
        let folded = fold_facts::<ClaudeFold>(&*self.log)
            .await
            .map_err(store_error)?;
        self.synced.take();
        self.folded.take();
        self.folded.put(folded);
        Ok(())
    }

    pub(crate) fn take_synced(&self) -> Vec<u64> {
        self.synced.take().unwrap_or_default()
    }

    pub(crate) fn put_synced(&self, synced: Vec<u64>) {
        self.synced.put(synced);
    }
}
