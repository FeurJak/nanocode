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
    contended: bool,
}

impl Board {
    pub async fn open(
        host: Arc<dyn LogHost>,
        scope: &str,
        identity: access::Identity,
    ) -> Result<Self, BoardError> {
        let rows = host.load(scope).await?;
        Self::start(host, scope, identity, Fold::new(), rows)
    }

    pub async fn resume(
        host: Arc<dyn LogHost>,
        scope: &str,
        identity: access::Identity,
        fold: Fold,
    ) -> Result<Self, BoardError> {
        let from = fold.next() + u64::from(mutated(Mutation::ResumeSkipsRows));
        let rows = host.scan(scope, from, END).await?;
        Self::start(host, scope, identity, fold, rows)
    }

    fn start(
        host: Arc<dyn LogHost>,
        scope: &str,
        identity: access::Identity,
        fold: Fold,
        rows: Vec<(u64, String)>,
    ) -> Result<Self, BoardError> {
        let mut board = Self {
            host,
            scope: scope.to_owned(),
            writer: identity.actor().to_string(),
            identity,
            minted: 0,
            fold,
            contended: false,
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
        let mut step = if self.contended {
            None
        } else {
            pending.optimistic(&self.fold)
        };
        loop {
            let step = match step.take() {
                Some(step) => step,
                None => {
                    let rows = self.host.scan(&self.scope, self.fold.next(), END).await;
                    if rows.as_ref().is_ok_and(Vec::is_empty) {
                        self.contended = false;
                    }
                    pending.read(&mut self.fold, rows)?
                }
            };
            match step {
                Step::Done(applied) => return Ok(applied),
                Step::Append { seq, body } => {
                    let appended = self.host.append(&self.scope, &[(seq, body)]).await;
                    self.contended |= appended.is_err();
                    if let Some(applied) = pending.acknowledged(&mut self.fold, seq, appended)? {
                        return Ok(applied);
                    }
                }
            }
        }
    }
}
