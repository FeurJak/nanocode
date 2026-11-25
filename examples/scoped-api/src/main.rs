mod drive;
mod http;
mod replica;

use std::{collections::BTreeMap, num::NonZeroU32, process::ExitCode, sync::Arc};

use nanocode_durability_core::{Sql, SqlStatement};
use nanocode_partitions::Partition;
use nanocode_store_sql::{PgOptions, PgPool, PgSql};
use nanocode_turnlog::parted::{PartStore, PartedLog, SqlParts};

const USAGE: &str = "scoped-api migrate URL [parts=N] | serve URL listen=HOST:PORT [parts= pool= lease_pool= threads= lease_ms= member_ms= grace_ms= tick_ms= renew_ms= batch= queue= forward_idle=] | drive ADMIN_URL replicas=A,B [parts= concurrency= scopes= seconds= writes= threads= retry_ms= pause_ms= sample_ms= ready_s= route=any|owner route_ms= kill=PID@S victim=ADDR | handoff=ADDR@S]";
const MEMBERS_CREATE: &str =
    "CREATE TABLE IF NOT EXISTS scoped_api_member (node TEXT PRIMARY KEY, seen_ms BIGINT NOT NULL)";
const MEMBERS_CHECK: &str = "SELECT CAST(count(*) AS TEXT) FROM scoped_api_member";
const PARTS_DEFAULT: u64 = 32;
const PART_ROWS_MAX: u64 = 512;

pub struct Options(BTreeMap<String, String>);

impl Options {
    pub fn number(&self, key: &str, default: u64) -> Result<u64, String> {
        self.0.get(key).map_or(Ok(default), |value| {
            value
                .parse()
                .map_err(|_| format!("{key}={value} is not a number"))
        })
    }

    pub fn text(&self, key: &str) -> Option<&str> {
        self.0.get(key).map(String::as_str)
    }

    pub fn parts(&self) -> Result<NonZeroU32, String> {
        let parts = self.number("parts", PARTS_DEFAULT)?;
        u32::try_from(parts)
            .ok()
            .and_then(NonZeroU32::new)
            .ok_or_else(|| format!("parts={parts} is not a partition count"))
    }
}

pub fn pg(url: &str, connections: usize) -> Result<Arc<PgSql>, String> {
    let options = PgOptions::parse(url).map_err(|error| format!("bad url: {error}"))?;
    let pool = PgPool::new(options, connections);
    Ok(Arc::new(PgSql::reading(pool.clone(), pool)))
}

pub fn runtime(threads: u64) -> Result<nanocode_executor::Runtime, String> {
    let threads = usize::try_from(threads).map_err(|_| "threads does not fit".to_owned())?;
    nanocode_executor::Builder::new_multi_thread()
        .worker_threads(threads)
        .build()
        .map_err(|error| format!("runtime: {error}"))
}

fn store(sql: &Arc<PgSql>) -> Arc<SqlParts<PgSql>> {
    Arc::new(SqlParts::new(Arc::clone(sql)).with_notify(false))
}

fn migrate(url: &str, options: &Options) -> Result<(), String> {
    let parts = options.parts()?;
    let sql = pg(url, 1)?;
    runtime(1)?.block_on(async {
        PartedLog::new(store(&sql), parts)
            .migrate()
            .await
            .map_err(|error| format!("migrate log: {error}"))?;
        sql.transaction(
            "scoped_api_migrate",
            &[SqlStatement {
                sql: MEMBERS_CREATE,
                params: Vec::new(),
            }],
        )
        .await
        .map_err(|error| format!("migrate members: {error}"))?;
        println!("migrated parts={parts}");
        Ok(())
    })
}

fn serve(url: &str, options: &Options) -> Result<(), String> {
    let parts = options.parts()?;
    let listen = options
        .text("listen")
        .ok_or("serve needs listen=HOST:PORT, the address peers forward to")?
        .to_owned();
    let lease_ms = options.number("lease_ms", 4_000)?;
    let size = |key: &str, default: u64| -> Result<usize, String> {
        usize::try_from(options.number(key, default)?).map_err(|_| format!("{key} does not fit"))
    };
    let settings = replica::Settings {
        listen: listen.clone(),
        parts,
        lease_ms,
        member_ms: options.number("member_ms", 2_000)?,
        grace_ms: options.number("grace_ms", 0)?,
        tick_ms: options.number("tick_ms", 100)?,
        renew_ms: options.number("renew_ms", lease_ms / 3)?,
        batch: size("batch", PART_ROWS_MAX)?,
        queue: size("queue", 4_096)?,
        forward_idle: size("forward_idle", 64)?,
    };
    let data = pg(url, size("pool", 8)?)?;
    let leases = pg(url, size("lease_pool", 2)?)?;
    let log = Arc::new(PartedLog::fenced(store(&leases), parts, &listen));
    runtime(options.number("threads", 2)?)?.block_on(async {
        let last = Partition(parts.get() - 1);
        log.store()
            .fence(last)
            .await
            .map_err(|error| format!("schema behind (run migrate parts={parts}): {error}"))?;
        leases
            .transaction(
                "scoped_api_check",
                &[SqlStatement {
                    sql: MEMBERS_CHECK,
                    params: Vec::new(),
                }],
            )
            .await
            .map_err(|error| format!("schema behind (run migrate): {error}"))?;
        eprintln!("scoped-api serving on {listen} parts={parts}");
        let stores = replica::Stores {
            data: store(&data),
            leases,
            log,
        };
        replica::serve(stores, settings).await
    })
}

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let command = args.next().unwrap_or_default();
    let (mut url, mut values) = (None, BTreeMap::new());
    for arg in args {
        match arg.split_once('=') {
            Some((key, value)) if !arg.starts_with("postgres") => {
                values.insert(key.to_owned(), value.to_owned());
            }
            _ => url = Some(arg),
        }
    }
    let options = Options(values);
    let result = match (command.as_str(), url) {
        ("migrate", Some(url)) => migrate(&url, &options),
        ("serve", Some(url)) => serve(&url, &options),
        ("drive", Some(url)) => drive::run(&url, &options),
        _ => Err(USAGE.to_owned()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("scoped-api: {error}");
            ExitCode::FAILURE
        }
    }
}
