use std::{future::Future, pin::pin, task::Poll};

use nanocode_durability_core::StoreError;
use nanocode_partitions::Partition;
use nanocode_sync::Semaphore;

use super::{PartStore, PartedLog, sim};

/// A partition this node claimed and recovered: everything at or before cursor was committed by
/// earlier owners.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Gained {
    pub part: Partition,
    pub epoch: u64,
    pub cursor: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GainError {
    /// No recovery capacity came free before the deadline; nothing was claimed.
    Busy,
    /// The capacity semaphore was closed; nothing was claimed.
    Closed,
    /// The claim was refused (a live owner holds the lease), or the lease moved while the
    /// partition recovered.
    Fenced,
    /// The store failed during the claim or the recovery.
    Store(StoreError),
}

impl<S: PartStore + ?Sized> PartedLog<S> {
    /// Gains one partition: waits for a permit of capacity until deadline resolves (Busy, nothing
    /// claimed), claims the lease, then runs recover from the cursor the claim handed over while
    /// holding the permit, so capacity bounds how many partitions recover at once and nothing
    /// else orders them. Run it in a task per gained partition and serve each partition as soon
    /// as its gain returns, whatever the others are doing.
    pub async fn gain<D, R, F, T>(
        &self,
        part: Partition,
        ttl_ms: u64,
        capacity: &Semaphore,
        deadline: D,
        recover: R,
    ) -> Result<(Gained, T), GainError>
    where
        D: Future<Output = ()>,
        R: FnOnce(Gained) -> F,
        F: Future<Output = Result<T, StoreError>>,
    {
        let mut acquire = pin!(capacity.acquire());
        let mut deadline = pin!(deadline);
        let permit = std::future::poll_fn(|context| {
            if let Poll::Ready(permit) = acquire.as_mut().poll(context) {
                return Poll::Ready(Some(permit));
            }
            if !sim::mutated(sim::Mutation::GainWaitsUnbounded)
                && deadline.as_mut().poll(context).is_ready()
            {
                return Poll::Ready(None);
            }
            Poll::Pending
        })
        .await
        .ok_or(GainError::Busy)?
        .map_err(|_| GainError::Closed)?;
        let epoch = self.claim(part, ttl_ms).await.map_err(|error| match error {
            StoreError::Fenced => GainError::Fenced,
            error => GainError::Store(error),
        })?;
        let cursor = self.resume(part).ok_or(GainError::Fenced)?;
        let gained = Gained {
            part,
            epoch,
            cursor,
        };
        let recovered = recover(gained).await.map_err(GainError::Store)?;
        drop(permit);
        if self.held(part) != Some(epoch) {
            return Err(GainError::Fenced);
        }
        Ok((gained, recovered))
    }
}
