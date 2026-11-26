use std::sync::Arc;

use nanocode_durability_core::{StoreError, StoreFuture};
use nanocode_partitions::{Partition, Row};
use nanocode_turnlog::parted::{PartStore, PartedLog};

pub trait Leases: Send + Sync {
    fn node(&self) -> &str;

    fn partition(&self, scope: &str) -> Partition;

    fn held(&self, part: Partition) -> Option<u64>;

    fn row(&self, part: Partition) -> StoreFuture<'_, Result<Row<String>, StoreError>>;

    fn claim(&self, part: Partition) -> StoreFuture<'_, Result<u64, StoreError>>;

    fn release(&self, part: Partition) -> StoreFuture<'_, Result<(), StoreError>>;

    fn renew(&self) -> StoreFuture<'_, Result<Vec<Partition>, StoreError>>;
}

pub struct PartedLeases<S: ?Sized> {
    log: Arc<PartedLog<S>>,
    node: String,
    ttl_ms: u64,
}

impl<S: PartStore + ?Sized> PartedLeases<S> {
    pub fn new(log: Arc<PartedLog<S>>, node: &str, ttl_ms: u64) -> Self {
        Self {
            log,
            node: node.to_owned(),
            ttl_ms,
        }
    }

    pub const fn log(&self) -> &Arc<PartedLog<S>> {
        &self.log
    }

    async fn claimed(&self, part: Partition) -> Result<u64, StoreError> {
        let epoch = self.log.claim(part, self.ttl_ms).await?;
        match self.log.store().fence(part).await? {
            Row::Held {
                holder,
                epoch: held,
                ..
            } if held == epoch && holder != self.node => Err(StoreError::Backend(format!(
                "the log claims partition {} for {holder}, not for the node {} its scopes run as",
                part.0, self.node
            ))),
            _ => Ok(epoch),
        }
    }
}

impl<S: PartStore + ?Sized + 'static> Leases for PartedLeases<S> {
    fn node(&self) -> &str {
        &self.node
    }

    fn partition(&self, scope: &str) -> Partition {
        self.log.partition(scope)
    }

    fn held(&self, part: Partition) -> Option<u64> {
        self.log.held(part)
    }

    fn row(&self, part: Partition) -> StoreFuture<'_, Result<Row<String>, StoreError>> {
        self.log.store().fence(part)
    }

    fn claim(&self, part: Partition) -> StoreFuture<'_, Result<u64, StoreError>> {
        Box::pin(self.claimed(part))
    }

    fn release(&self, part: Partition) -> StoreFuture<'_, Result<(), StoreError>> {
        Box::pin(self.log.release(part))
    }

    fn renew(&self) -> StoreFuture<'_, Result<Vec<Partition>, StoreError>> {
        Box::pin(self.log.renew_all(self.ttl_ms))
    }
}
