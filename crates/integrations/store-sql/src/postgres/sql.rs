use crate::postgres::{PgConnection, PgError, PgPool};
use nanocode_durability_core::{
    Settle, Sql, SqlAccess, SqlDialect, SqlRow, SqlStatement, StoreError, StoreFuture,
};
use nanocode_sync::{Actor, Stopped};
use std::{sync::Arc, time::Duration};

const RESET_SESSION: &str = "RESET ALL";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PgTimes {
    pub waited: Duration,
    pub ran: Duration,
}

pub struct PgSql {
    writes: PgPool,
    reads: PgPool,
    timed: Option<fn(&'static str, PgTimes)>,
}

impl PgSql {
    #[must_use]
    pub const fn reading(writes: PgPool, reads: PgPool) -> Self {
        Self {
            writes,
            reads,
            timed: None,
        }
    }

    #[must_use]
    pub fn timed(mut self, timed: fn(&'static str, PgTimes)) -> Self {
        self.timed = Some(timed);
        self
    }

    async fn transact(
        &self,
        pool: &PgPool,
        operation: &'static str,
        settle: Settle,
        statements: &[SqlStatement<'_>],
    ) -> Result<Vec<Vec<SqlRow>>, StoreError> {
        let started = nanocode_platform::now();
        let mut connection = pool.acquire().await.map_err(backend)?;
        let acquired = nanocode_platform::now();
        let result = connection
            .transaction(settle, statements)
            .await
            .map_err(backend);
        if let Some(timed) = self.timed {
            timed(
                operation,
                PgTimes {
                    waited: acquired - started,
                    ran: nanocode_platform::elapsed(acquired),
                },
            );
        }
        result
    }
}

impl PgSql {
    async fn autocommit(
        &self,
        operation: &'static str,
        statements: &[SqlStatement<'_>],
    ) -> Result<Vec<Vec<SqlRow>>, StoreError> {
        let started = nanocode_platform::now();
        let mut connection = self.writes.acquire().await.map_err(backend)?;
        let acquired = nanocode_platform::now();
        let mut results = Vec::with_capacity(statements.len());
        let mut failed = None;
        for statement in statements {
            match connection.run(std::slice::from_ref(statement)).await {
                Ok(answer) => results.extend(answer),
                Err(error) => {
                    failed = Some(error);
                    break;
                }
            }
        }
        let reset = connection
            .run(&[SqlStatement {
                sql: RESET_SESSION,
                params: Vec::new(),
            }])
            .await;
        if let Some(timed) = self.timed {
            timed(
                operation,
                PgTimes {
                    waited: acquired - started,
                    ran: nanocode_platform::elapsed(acquired),
                },
            );
        }
        if let Some(error) = failed {
            return Err(backend(error));
        }
        reset.map_err(backend)?;
        Ok(results)
    }
}

impl Sql for PgSql {
    fn transaction<'a>(
        &'a self,
        operation: &'static str,
        statements: &'a [SqlStatement<'a>],
    ) -> StoreFuture<'a, Result<Vec<Vec<SqlRow>>, StoreError>> {
        Box::pin(self.transact(&self.writes, operation, Settle::Durable, statements))
    }

    fn call_bound(&self) -> Option<Duration> {
        None
    }

    fn run<'a>(
        &'a self,
        access: SqlAccess,
        operation: &'static str,
        statements: &'a [SqlStatement<'a>],
    ) -> StoreFuture<'a, Result<Vec<Vec<SqlRow>>, StoreError>> {
        let pool = match access {
            SqlAccess::Read => &self.reads,
            SqlAccess::Write | SqlAccess::Schema => &self.writes,
            SqlAccess::Autocommit => return Box::pin(self.autocommit(operation, statements)),
        };
        Box::pin(self.transact(pool, operation, Settle::Durable, statements))
    }

    fn write<'a>(
        &'a self,
        settle: Settle,
        operation: &'static str,
        statements: &'a [SqlStatement<'a>],
    ) -> StoreFuture<'a, Result<Vec<Vec<SqlRow>>, StoreError>> {
        Box::pin(self.transact(&self.writes, operation, settle, statements))
    }

    fn dialect(&self) -> SqlDialect {
        SqlDialect::Postgres
    }

    fn concurrent(self: Arc<Self>) -> Option<Arc<dyn Sql>> {
        (self.writers() > 1 || self.readers() > 1).then_some(self as Arc<dyn Sql>)
    }

    fn writers(&self) -> usize {
        self.writes.connections()
    }

    fn readers(&self) -> usize {
        self.reads.connections()
    }
}

pub struct PgTransaction(Actor<Option<PgConnection>>);

impl PgTransaction {
    pub fn new(connection: PgConnection) -> std::io::Result<Self> {
        Actor::spawn(Some(connection)).map(Self)
    }

    pub async fn finish(self) -> Result<PgConnection, Stopped> {
        let connection = self
            .0
            .run(|connection| Box::pin(std::future::ready(connection.take())))
            .await?;
        Ok(connection.expect("a transaction holds its connection until it finishes"))
    }
}

impl Sql for PgTransaction {
    fn transaction<'a>(
        &'a self,
        _operation: &'static str,
        statements: &'a [SqlStatement<'a>],
    ) -> StoreFuture<'a, Result<Vec<Vec<SqlRow>>, StoreError>> {
        let statements: Vec<(&'static str, Vec<String>)> = statements
            .iter()
            .map(|statement| {
                let params = statement.params.iter().map(|&param| param.to_owned());
                (statement.sql, params.collect())
            })
            .collect();
        let ran = self.0.run(move |connection| {
            Box::pin(async move {
                let statements: Vec<SqlStatement<'_>> = statements
                    .iter()
                    .map(|(sql, params)| SqlStatement {
                        sql,
                        params: params.iter().map(String::as_str).collect(),
                    })
                    .collect();
                connection
                    .as_mut()
                    .expect("a transaction holds its connection until it finishes")
                    .run(&statements)
                    .await
            })
        });
        Box::pin(async move {
            ran.await
                .map_err(|stopped| StoreError::Backend(stopped.to_string()))?
                .map_err(backend)
        })
    }

    fn call_bound(&self) -> Option<Duration> {
        None
    }

    fn dialect(&self) -> SqlDialect {
        SqlDialect::Postgres
    }
}

fn backend(error: PgError) -> StoreError {
    StoreError::Backend(error.to_string())
}
