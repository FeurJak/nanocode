use std::sync::Arc;

use nanocode_cost_fold::{Applied, CostError, CostEvent, Ledger};
use nanocode_durability_core::StoreError;
use nanocode_turnlog::LogHost;

use crate::END;

#[derive(Clone, Debug)]
pub enum CostLogError {
    Store(StoreError),
    Corrupt { seq: u64, error: CostError },
    Rejected(CostError),
    Unsettled { seq: u64, error: StoreError },
}

impl std::fmt::Display for CostLogError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Store(error) => write!(formatter, "cost store: {error}"),
            Self::Corrupt { seq, error } => write!(formatter, "cost row {seq}: {error}"),
            Self::Rejected(error) => write!(formatter, "cost write rejected: {error}"),
            Self::Unsettled { seq, error } => {
                write!(formatter, "cost write at row {seq} is unsettled: {error}")
            }
        }
    }
}

impl From<StoreError> for CostLogError {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

pub struct CostLog {
    host: Arc<dyn LogHost>,
    scope: String,
    next: u64,
    ledger: Ledger,
}

impl CostLog {
    pub async fn open(host: Arc<dyn LogHost>, scope: &str) -> Result<Self, CostLogError> {
        let rows = host.load(scope).await?;
        let mut log = Self {
            host,
            scope: scope.to_owned(),
            next: 1,
            ledger: Ledger::new(),
        };
        log.fold_rows(rows)?;
        Ok(log)
    }

    pub const fn ledger(&self) -> &Ledger {
        &self.ledger
    }

    pub fn scope(&self) -> &str {
        &self.scope
    }

    fn fold_rows(&mut self, rows: Vec<(u64, String)>) -> Result<(), CostLogError> {
        for (seq, body) in rows {
            self.ledger
                .apply_json(&body)
                .map_err(|error| CostLogError::Corrupt { seq, error })?;
            self.next = seq + 1;
        }
        Ok(())
    }

    pub async fn refresh(&mut self) -> Result<(), CostLogError> {
        let rows = self.host.scan(&self.scope, self.next, END).await?;
        self.fold_rows(rows)
    }

    pub async fn record(&mut self, event: CostEvent) -> Result<Applied, CostLogError> {
        let body = serde_json::to_string(&event).map_err(|error| {
            CostLogError::Rejected(CostError::Corrupt {
                detail: error.to_string(),
            })
        })?;
        let mut appended = false;
        loop {
            self.refresh().await?;
            match self.ledger.check(&event).map_err(CostLogError::Rejected)? {
                Applied::Duplicate if appended => return Ok(Applied::Recorded),
                Applied::Duplicate => return Ok(Applied::Duplicate),
                Applied::Recorded => {}
            }
            let seq = self.next;
            match self.host.append(&self.scope, &[(seq, body.clone())]).await {
                Ok(()) => appended = true,
                Err(error) => {
                    let scanned = self.host.scan(&self.scope, self.next, END).await;
                    let rows = scanned.map_err(|_| CostLogError::Unsettled {
                        seq,
                        error: error.clone(),
                    })?;
                    self.fold_rows(rows)?;
                    if self.next <= seq {
                        return Err(CostLogError::Unsettled { seq, error });
                    }
                    if matches!(self.ledger.check(&event), Ok(Applied::Duplicate)) {
                        return Ok(Applied::Recorded);
                    }
                }
            }
        }
    }
}
