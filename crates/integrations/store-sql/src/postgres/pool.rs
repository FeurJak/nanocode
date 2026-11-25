use crate::postgres::{
    PgConnection, PgError, PgLimits, PgOptions,
    connection::{PgCancel, Reuse},
    sim,
};
use nanocode_platform::Elapsed;
use nanocode_sync::{Mailbox, OwnedSemaphorePermit, Semaphore};
use std::{
    future::Future,
    ops::{Deref, DerefMut},
    sync::Arc,
    time::Duration,
};

#[derive(Clone)]
pub struct PgPool(Arc<Shared>);

struct Shared {
    options: PgOptions,
    limits: PgLimits,
    reuse: Reuse,
    lending: Lending<PgConnection>,
}

pub type Pooled = Lent<PgConnection>;

pub struct Lending<C> {
    connections: usize,
    idle: Mailbox<Vec<C>>,
    permits: Arc<Semaphore>,
}

pub struct Lent<C: Send + 'static> {
    connection: Option<C>,
    idle: Mailbox<Vec<C>>,
    _permit: OwnedSemaphorePermit,
}

impl PgPool {
    #[must_use]
    pub fn new(options: PgOptions, connections: usize) -> Self {
        Self::limited(options, connections, PgLimits::default())
    }

    #[must_use]
    pub fn limited(options: PgOptions, connections: usize, limits: PgLimits) -> Self {
        limits.assert_bounded();
        Self(Arc::new(Shared {
            options,
            limits,
            reuse: Reuse::default(),
            lending: Lending::new(connections),
        }))
    }

    #[must_use]
    pub fn connections(&self) -> usize {
        self.0.lending.connections()
    }

    #[must_use]
    pub fn options(&self) -> &PgOptions {
        &self.0.options
    }

    #[must_use]
    pub fn limits(&self) -> &PgLimits {
        &self.0.limits
    }

    pub async fn acquire(&self) -> Result<Pooled, PgError> {
        let shared = &self.0;
        shared
            .lending
            .lend(shared.limits.acquire, PgConnection::is_ready, || {
                PgConnection::connect_reusing(&shared.options, &shared.reuse)
            })
            .await
            .map_err(|Elapsed| PgError::Busy)?
    }

    pub async fn cancel(&self, key: &PgCancel) -> Result<(), PgError> {
        let shared = &self.0;
        key.send(&shared.options, &shared.reuse, shared.limits.acquire)
            .await
    }
}

impl<C: Send + 'static> Lending<C> {
    #[must_use]
    pub fn new(connections: usize) -> Self {
        assert!(connections > 0, "a pool holds at least one connection");
        Self {
            connections,
            idle: Mailbox::new(Vec::with_capacity(connections)),
            permits: Arc::new(Semaphore::new(connections)),
        }
    }

    #[must_use]
    pub const fn connections(&self) -> usize {
        self.connections
    }

    pub async fn lend<E, F: Future<Output = Result<C, E>>>(
        &self,
        wait: Duration,
        ready: fn(&C) -> bool,
        connect: impl FnOnce() -> F,
    ) -> Result<Result<Lent<C>, E>, Elapsed> {
        let deadline =
            (!sim::mutated(sim::Mutation::WaitsUnbounded)).then(|| nanocode_platform::now() + wait);
        let permit = by(deadline, Arc::clone(&self.permits).acquire_owned())
            .await?
            .expect("a pool never closes its semaphore");
        let connections = self.connections;
        let idle = by(
            deadline,
            self.idle.call(move |idle| {
                assert!(
                    idle.len() <= connections,
                    "a pool idles at most its connections"
                );
                idle.pop()
            }),
        )
        .await?;
        let mut lent = Lent {
            connection: idle.filter(ready),
            idle: self.idle.clone(),
            _permit: permit,
        };
        if lent.connection.is_none() {
            match by(deadline, connect()).await? {
                Ok(connection) => lent.connection = Some(connection),
                Err(error) => return Ok(Err(error)),
            }
        }
        Ok(Ok(lent))
    }
}

async fn by<F: Future>(
    deadline: Option<nanocode_platform::Instant>,
    future: F,
) -> Result<F::Output, Elapsed> {
    match deadline {
        Some(deadline) => nanocode_platform::timeout_at(deadline, future).await,
        None => Ok(future.await),
    }
}

impl<C: Send + 'static> Lent<C> {
    pub fn discard(mut self) {
        drop(self.connection.take());
    }
}

impl<C: Send + 'static> Drop for Lent<C> {
    fn drop(&mut self) {
        if let Some(connection) = self.connection.take() {
            self.idle.send(move |idle| idle.push(connection));
        }
    }
}

impl<C: Send + 'static> Deref for Lent<C> {
    type Target = C;

    fn deref(&self) -> &C {
        self.connection
            .as_ref()
            .expect("a pooled connection is present while it is lent")
    }
}

impl<C: Send + 'static> DerefMut for Lent<C> {
    fn deref_mut(&mut self) -> &mut C {
        self.connection
            .as_mut()
            .expect("a pooled connection is present while it is lent")
    }
}
