use std::{future::Future, sync::Arc};

use nanocode_scope::ScopeError;
use nanocode_sync::Atom;

use super::{ThreadLog, ThreadLogs, sim};

pub(crate) struct ThreadBinding {
    logs: Arc<ThreadLogs>,
    thread: Arc<str>,
    log: Atom<Arc<ThreadLog>>,
}

impl ThreadBinding {
    pub(crate) fn new(logs: Arc<ThreadLogs>, thread: &str, log: Arc<ThreadLog>) -> Self {
        Self {
            logs,
            thread: Arc::from(thread),
            log: Atom::new(log),
        }
    }

    pub(crate) fn log(&self) -> Arc<ThreadLog> {
        self.log.read(Arc::clone)
    }

    pub(crate) async fn rebound(
        &self,
        stale: &Arc<ThreadLog>,
    ) -> Result<Arc<ThreadLog>, ScopeError> {
        let current = self.log();
        if !Arc::ptr_eq(&current, stale) {
            return Ok(current);
        }
        let fresh = Arc::new(self.logs.get(&self.thread).await?);
        Ok(self.log.update(|log| {
            if Arc::ptr_eq(log, stale) {
                *log = Arc::clone(&fresh);
            }
            Arc::clone(log)
        }))
    }
}

#[derive(Clone)]
pub(super) enum Bind {
    Fixed(Arc<ThreadLog>),
    Rebinds(Arc<ThreadBinding>),
}

impl Bind {
    pub(super) fn log(&self) -> Arc<ThreadLog> {
        match self {
            Self::Fixed(log) => Arc::clone(log),
            Self::Rebinds(binding) => binding.log(),
        }
    }

    pub(super) async fn again(
        &self,
        stale: &Arc<ThreadLog>,
        unretried: sim::Mutation,
    ) -> Option<Result<Arc<ThreadLog>, ScopeError>> {
        match self {
            Self::Rebinds(binding) if !sim::mutated(unretried) => {
                Some(binding.rebound(stale).await)
            }
            Self::Fixed(_) | Self::Rebinds(_) => None,
        }
    }

    pub(super) async fn serving(
        &self,
        unretried: sim::Mutation,
    ) -> Result<Arc<ThreadLog>, ScopeError> {
        let log = self.log();
        if !log.retired() {
            return Ok(log);
        }
        match self.again(&log, unretried).await {
            Some(fresh) => fresh,
            None => Ok(log),
        }
    }

    pub(super) async fn settled(
        &self,
        log: &Arc<ThreadLog>,
        unretried: sim::Mutation,
    ) -> Result<(), ScopeError> {
        if !log.retired() {
            return Ok(());
        }
        match self.again(log, unretried).await {
            Some(fresh) => fresh?.flush(None).await.map(drop),
            None => Ok(()),
        }
    }

    pub(super) async fn retried<T, F, Fut>(
        &self,
        unretried: sim::Mutation,
        command: F,
    ) -> Result<T, ScopeError>
    where
        F: Fn(Arc<ThreadLog>) -> Fut,
        Fut: Future<Output = Result<T, ScopeError>>,
    {
        let log = self.serving(unretried).await?;
        match command(Arc::clone(&log)).await {
            Err(ScopeError::Lost) => match self.again(&log, unretried).await {
                Some(fresh) => command(fresh?).await,
                None => Err(ScopeError::Lost),
            },
            answered => answered,
        }
    }
}
