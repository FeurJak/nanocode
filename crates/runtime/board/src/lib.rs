use std::sync::Arc;

use nanocode_turnlog::LogHost;

pub use nanocode_board_fold::*;

pub mod access;
pub mod cost;
pub mod tasks;

use sim::{Mutation, mutated};

const END: u64 = i64::MAX as u64;

pub struct Board {
    host: Arc<dyn LogHost>,
    scope: String,
    writer: String,
    identity: access::Identity,
    minted: u64,
    fold: Fold,
}

impl Board {
    pub async fn open(
        host: Arc<dyn LogHost>,
        scope: &str,
        identity: access::Identity,
    ) -> Result<Self, BoardError> {
        let rows = host.load(scope).await?;
        let mut board = Self {
            host,
            scope: scope.to_owned(),
            writer: identity.actor().to_string(),
            identity,
            minted: 0,
            fold: Fold::new(),
        };
        board.fold_rows(rows)?;
        if !mutated(Mutation::MintedRestarts) {
            board.minted = board.fold.minted(&board.writer);
        }
        Ok(board)
    }

    pub const fn fold(&self) -> &Fold {
        &self.fold
    }

    pub fn writer(&self) -> &str {
        &self.writer
    }

    fn fold_rows(&mut self, rows: Vec<(u64, String)>) -> Result<(), BoardError> {
        for (seq, body) in rows {
            self.fold.apply(seq, &body)?;
        }
        Ok(())
    }

    pub async fn refresh(&mut self) -> Result<(), BoardError> {
        let rows = self.host.scan(&self.scope, self.fold.next(), END).await?;
        self.fold_rows(rows)
    }

    pub async fn submit(&mut self, at_ms: u64, op: Op) -> Result<Applied, BoardError> {
        self.minted += 1;
        let id = EventId(format!("{}.{}", self.writer, self.minted));
        self.submit_as(id, at_ms, op).await
    }

    pub async fn submit_keyed(
        &mut self,
        key: &str,
        at_ms: u64,
        op: Op,
    ) -> Result<Applied, BoardError> {
        if mutated(Mutation::KeysIgnored) {
            return self.submit(at_ms, op).await;
        }
        self.submit_as(EventId(format!("{}#{key}", self.writer)), at_ms, op)
            .await
    }

    async fn submit_as(&mut self, id: EventId, at_ms: u64, op: Op) -> Result<Applied, BoardError> {
        let mut pending = self.fold.submit(Event {
            id,
            by: self.writer.clone(),
            at_ms,
            op,
        })?;
        loop {
            let rows = self.host.scan(&self.scope, self.fold.next(), END).await;
            match pending.read(&mut self.fold, rows)? {
                Step::Done(applied) => return Ok(applied),
                Step::Append { seq, body } => {
                    let appended = self.host.append(&self.scope, &[(seq, body)]).await;
                    pending.appended(seq, appended)?;
                }
            }
        }
    }
}
