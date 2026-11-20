pub use nanocode_board_fold::access::*;

use crate::{Applied, Board, BoardError, Entry, EventId, Op, Task, TaskId};

impl Board {
    pub const fn identity(&self) -> &Identity {
        &self.identity
    }

    pub fn may(&self, channel: &str, can: Can) -> bool {
        self.fold.access().may(self.identity.actor(), channel, can)
    }

    pub fn tasks(&self) -> impl Iterator<Item = (&TaskId, &Task)> + '_ {
        self.fold.view(self.identity.actor())
    }

    pub fn task(&self, id: &TaskId) -> Option<&Task> {
        self.fold
            .task(id)
            .filter(|_| self.fold.access().reads(self.identity.actor(), &id.0))
    }

    pub fn entries(&self) -> impl Iterator<Item = &Entry> + '_ {
        self.fold.seen_by(self.identity.actor())
    }

    pub fn ready(&self, at_ms: u64) -> Vec<TaskId> {
        self.fold
            .ready(at_ms)
            .filter(|id| self.may(&id.0, Can::Label))
            .cloned()
            .collect()
    }

    pub async fn charter(&mut self, at_ms: u64) -> Result<Applied, BoardError> {
        self.submit_keyed("charter", at_ms, Op::Charter {}).await
    }

    pub async fn grant(
        &mut self,
        key: &str,
        at_ms: u64,
        grant: Grant,
    ) -> Result<Applied, BoardError> {
        self.submit_keyed(key, at_ms, Op::Grant { grant }).await
    }

    pub async fn revoke(
        &mut self,
        key: &str,
        at_ms: u64,
        grant: EventId,
    ) -> Result<Applied, BoardError> {
        self.submit_keyed(key, at_ms, Op::Revoke { grant }).await
    }
}
