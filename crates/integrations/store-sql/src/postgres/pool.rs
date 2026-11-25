use crate::postgres::{PgConnection, PgError, PgOptions, connection::Reuse};
use nanocode_sync::{Mailbox, OwnedSemaphorePermit, Semaphore};
use std::{
    ops::{Deref, DerefMut},
    sync::Arc,
};

#[derive(Clone)]
pub struct PgPool(Arc<Shared>);

struct Shared {
    options: PgOptions,
    reuse: Reuse,
    connections: usize,
    idle: Mailbox<Vec<PgConnection>>,
    permits: Arc<Semaphore>,
}

pub struct Pooled {
    connection: Option<PgConnection>,
    idle: Mailbox<Vec<PgConnection>>,
    _permit: OwnedSemaphorePermit,
}

impl PgPool {
    #[must_use]
    pub fn new(options: PgOptions, connections: usize) -> Self {
        assert!(connections > 0, "a pool holds at least one connection");
        Self(Arc::new(Shared {
            options,
            reuse: Reuse::default(),
            connections,
            idle: Mailbox::new(Vec::with_capacity(connections)),
            permits: Arc::new(Semaphore::new(connections)),
        }))
    }

    #[must_use]
    pub fn connections(&self) -> usize {
        self.0.connections
    }

    #[must_use]
    pub fn options(&self) -> &PgOptions {
        &self.0.options
    }

    pub async fn acquire(&self) -> Result<Pooled, PgError> {
        let permit = Arc::clone(&self.0.permits)
            .acquire_owned()
            .await
            .expect("a pool never closes its semaphore");
        let connections = self.0.connections;
        let idle = self
            .0
            .idle
            .call(move |idle| {
                assert!(idle.len() < connections, "a permit leaves a connection free");
                idle.pop()
            })
            .await;
        let mut pooled = Pooled {
            connection: idle.filter(PgConnection::is_ready),
            idle: self.0.idle.clone(),
            _permit: permit,
        };
        if pooled.connection.is_none() {
            pooled.connection =
                Some(PgConnection::connect_reusing(&self.0.options, &self.0.reuse).await?);
        }
        Ok(pooled)
    }
}

impl Drop for Pooled {
    fn drop(&mut self) {
        if let Some(connection) = self.connection.take() {
            self.idle.send(move |idle| idle.push(connection));
        }
    }
}

impl Deref for Pooled {
    type Target = PgConnection;

    fn deref(&self) -> &PgConnection {
        self.connection
            .as_ref()
            .expect("a pooled connection is present while it is lent")
    }
}

impl DerefMut for Pooled {
    fn deref_mut(&mut self) -> &mut PgConnection {
        self.connection
            .as_mut()
            .expect("a pooled connection is present while it is lent")
    }
}
