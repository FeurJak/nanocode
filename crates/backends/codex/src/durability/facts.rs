use std::sync::Arc;

use nanocode_agent_core::{NanocodeError, Result};
use nanocode_durability::facts::{FactLog, Folded, OwnedFact, commit_fact, fold_facts};

use super::{CodexDecision, fold::CodexFold};

#[doc(hidden)]
pub struct CodexFacts {
    log: Arc<dyn FactLog>,
    folded: nanocode_sync::Slot<Folded<CodexFold>>,
    order: nanocode_sync::Sequencer,
}

fn store_error(error: nanocode_durability::StoreError) -> NanocodeError {
    nanocode_durability::agent_store_error(error)
}

impl CodexFacts {
    pub(crate) async fn open(log: Arc<dyn FactLog>) -> Result<Self> {
        let folded = fold_facts::<CodexFold>(&*log).await.map_err(store_error)?;
        Ok(Self {
            log,
            folded: nanocode_sync::Slot::new(Some(folded)),
            order: nanocode_sync::Sequencer::new(),
        })
    }

    pub(crate) async fn read<R>(&self, read: impl FnOnce(&CodexFold) -> R) -> Result<R> {
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

    fn taken(&self) -> Result<Folded<CodexFold>> {
        self.folded.take().ok_or_else(|| {
            NanocodeError::InvalidExecutionPolicy(
                "the Codex fact log could not be refolded after a failed commit".into(),
            )
        })
    }

    pub(crate) async fn commit(&self, fact: OwnedFact<CodexDecision>) -> Result<u64> {
        let _turn = self.order.enter().await;
        let folded = self.taken()?;
        match commit_fact(&*self.log, folded, fact).await {
            Ok((folded, seq)) => {
                self.folded.put(folded);
                Ok(seq)
            }
            Err(error) => {
                if let Ok(folded) = fold_facts::<CodexFold>(&*self.log).await {
                    self.folded.put(folded);
                }
                Err(store_error(error))
            }
        }
    }
}
