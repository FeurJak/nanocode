use crate::{
    model::{Corrupt, Digest, Entry, Manifest, Refusal},
    sim::{Mutation, mutated},
    store::{Batch, Blob, Failure, Store},
};
use nanocode_bytes::Bytes;
use nanocode_durability_core::{
    Migration, Sql, SqlAccess, SqlDialect, SqlRow, SqlStatement, StoreError, StoreFuture,
    migrate_format,
};
use nanocode_sys::base64::STANDARD;
use serde_json::Value;
use std::{collections::BTreeMap, sync::Arc};

type Dialects = [&'static str; 2];

const COMPONENT: &str = "workspace_fs";

const SCHEMA_SQLITE: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS nanocode_workspace_heads (workspace TEXT PRIMARY KEY, version INTEGER NOT NULL, token TEXT NOT NULL)",
    "CREATE TABLE IF NOT EXISTS nanocode_workspace_entries (workspace TEXT NOT NULL, path TEXT NOT NULL, kind TEXT NOT NULL, digest TEXT, size INTEGER NOT NULL, mode INTEGER NOT NULL, mtime INTEGER NOT NULL, target TEXT, PRIMARY KEY (workspace, path))",
    "CREATE TABLE IF NOT EXISTS nanocode_workspace_blobs (digest TEXT PRIMARY KEY, data TEXT NOT NULL, refs INTEGER NOT NULL)",
];

const SCHEMA_POSTGRES: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS nanocode_workspace_heads (workspace TEXT PRIMARY KEY, version BIGINT NOT NULL, token TEXT NOT NULL)",
    "CREATE TABLE IF NOT EXISTS nanocode_workspace_entries (workspace TEXT NOT NULL, path TEXT NOT NULL, kind TEXT NOT NULL, digest TEXT, size BIGINT NOT NULL, mode BIGINT NOT NULL, mtime BIGINT NOT NULL, target TEXT, PRIMARY KEY (workspace, path))",
    "CREATE TABLE IF NOT EXISTS nanocode_workspace_blobs (digest TEXT PRIMARY KEY, data TEXT NOT NULL, refs BIGINT NOT NULL)",
];

const MIGRATIONS: [Migration; 1] = [Migration {
    statements: [SCHEMA_SQLITE, SCHEMA_POSTGRES],
    adopt: None,
}];

const LOAD: Dialects = [
    "SELECT CAST(h.version AS TEXT), e.path, e.kind, e.digest, CAST(e.size AS TEXT), CAST(e.mode AS TEXT), CAST(e.mtime AS TEXT), e.target FROM nanocode_workspace_heads AS h LEFT JOIN nanocode_workspace_entries AS e ON e.workspace = h.workspace WHERE h.workspace = ?1 ORDER BY e.path",
    "SELECT CAST(h.version AS TEXT), e.path, e.kind, e.digest, CAST(e.size AS TEXT), CAST(e.mode AS TEXT), CAST(e.mtime AS TEXT), e.target FROM nanocode_workspace_heads AS h LEFT JOIN nanocode_workspace_entries AS e ON e.workspace = h.workspace WHERE h.workspace = $1::text ORDER BY e.path",
];

const CLAIM_FIRST: Dialects = [
    "INSERT INTO nanocode_workspace_heads (workspace, version, token) VALUES (?1, 1, ?2) ON CONFLICT (workspace) DO NOTHING",
    "INSERT INTO nanocode_workspace_heads (workspace, version, token) VALUES ($1::text, 1, $2::text) ON CONFLICT (workspace) DO NOTHING",
];

const CLAIM_NEXT: Dialects = [
    "UPDATE nanocode_workspace_heads SET version = version + 1, token = ?2 WHERE workspace = ?1 AND version = CAST(?3 AS INTEGER)",
    "UPDATE nanocode_workspace_heads SET version = version + 1, token = $2::text WHERE workspace = $1::text AND version = CAST($3::text AS BIGINT)",
];

const CLAIM_ANY: Dialects = [
    "UPDATE nanocode_workspace_heads SET version = version + 1, token = ?2 WHERE workspace = ?1 AND ?3 IS NOT NULL",
    "UPDATE nanocode_workspace_heads SET version = version + 1, token = $2::text WHERE workspace = $1::text AND $3::text IS NOT NULL",
];

const CLEAR: Dialects = [
    "DELETE FROM nanocode_workspace_entries WHERE workspace = ?1 AND path IN (SELECT value FROM json_each(?3)) AND EXISTS (SELECT 1 FROM nanocode_workspace_heads WHERE workspace = ?1 AND token = ?2)",
    "DELETE FROM nanocode_workspace_entries WHERE workspace = $1::text AND path IN (SELECT jsonb_array_elements_text($3::text::jsonb)) AND EXISTS (SELECT 1 FROM nanocode_workspace_heads WHERE workspace = $1::text AND token = $2::text)",
];

const PUT: Dialects = [
    "INSERT INTO nanocode_workspace_entries (workspace, path, kind, digest, size, mode, mtime, target) SELECT ?1, json_extract(value, '$[0]'), json_extract(value, '$[1]'), json_extract(value, '$[2]'), CAST(json_extract(value, '$[3]') AS INTEGER), CAST(json_extract(value, '$[4]') AS INTEGER), CAST(json_extract(value, '$[5]') AS INTEGER), json_extract(value, '$[6]') FROM json_each(?3) WHERE EXISTS (SELECT 1 FROM nanocode_workspace_heads WHERE workspace = ?1 AND token = ?2)",
    "INSERT INTO nanocode_workspace_entries (workspace, path, kind, digest, size, mode, mtime, target) SELECT $1::text, e->>0, e->>1, e->>2, CAST(e->>3 AS BIGINT), CAST(e->>4 AS BIGINT), CAST(e->>5 AS BIGINT), e->>6 FROM jsonb_array_elements($3::text::jsonb) AS e WHERE EXISTS (SELECT 1 FROM nanocode_workspace_heads WHERE workspace = $1::text AND token = $2::text)",
];

const ADD_BLOB: Dialects = [
    "INSERT INTO nanocode_workspace_blobs (digest, data, refs) SELECT ?3, ?4, 1 WHERE EXISTS (SELECT 1 FROM nanocode_workspace_heads WHERE workspace = ?1 AND token = ?2) ON CONFLICT (digest) DO UPDATE SET refs = nanocode_workspace_blobs.refs + 1",
    "INSERT INTO nanocode_workspace_blobs (digest, data, refs) SELECT $3::text, $4::text, 1 WHERE EXISTS (SELECT 1 FROM nanocode_workspace_heads WHERE workspace = $1::text AND token = $2::text) ON CONFLICT (digest) DO UPDATE SET refs = nanocode_workspace_blobs.refs + 1",
];

const RELEASE_BLOBS: Dialects = [
    "UPDATE nanocode_workspace_blobs SET refs = refs - 1 WHERE digest IN (SELECT value FROM json_each(?3)) AND EXISTS (SELECT 1 FROM nanocode_workspace_heads WHERE workspace = ?1 AND token = ?2)",
    "UPDATE nanocode_workspace_blobs SET refs = refs - 1 WHERE digest IN (SELECT jsonb_array_elements_text($3::text::jsonb)) AND EXISTS (SELECT 1 FROM nanocode_workspace_heads WHERE workspace = $1::text AND token = $2::text)",
];

const DROP_BLOBS: Dialects = [
    "DELETE FROM nanocode_workspace_blobs WHERE digest IN (SELECT value FROM json_each(?3)) AND refs <= 0 AND EXISTS (SELECT 1 FROM nanocode_workspace_heads WHERE workspace = ?1 AND token = ?2)",
    "DELETE FROM nanocode_workspace_blobs WHERE digest IN (SELECT jsonb_array_elements_text($3::text::jsonb)) AND refs <= 0 AND EXISTS (SELECT 1 FROM nanocode_workspace_heads WHERE workspace = $1::text AND token = $2::text)",
];

const RELEASE_EARLY: Dialects = [
    "UPDATE nanocode_workspace_blobs SET refs = refs - 1 WHERE digest IN (SELECT value FROM json_each(?3)) AND ?1 IS NOT NULL AND ?2 IS NOT NULL",
    "UPDATE nanocode_workspace_blobs SET refs = refs - 1 WHERE digest IN (SELECT jsonb_array_elements_text($3::text::jsonb)) AND $1::text IS NOT NULL AND $2::text IS NOT NULL",
];

const DROP_EARLY: Dialects = [
    "DELETE FROM nanocode_workspace_blobs WHERE digest IN (SELECT value FROM json_each(?3)) AND refs <= 0 AND ?1 IS NOT NULL AND ?2 IS NOT NULL",
    "DELETE FROM nanocode_workspace_blobs WHERE digest IN (SELECT jsonb_array_elements_text($3::text::jsonb)) AND refs <= 0 AND $1::text IS NOT NULL AND $2::text IS NOT NULL",
];

const HEAD: Dialects = [
    "SELECT CAST(version AS TEXT), token FROM nanocode_workspace_heads WHERE workspace = ?1",
    "SELECT CAST(version AS TEXT), token FROM nanocode_workspace_heads WHERE workspace = $1::text",
];

const BLOB: Dialects = [
    "SELECT data FROM nanocode_workspace_blobs WHERE digest = ?1",
    "SELECT data FROM nanocode_workspace_blobs WHERE digest = $1::text",
];

const FILE: &str = "file";
const DIR: &str = "dir";
const SYMLINK: &str = "symlink";

pub struct SqlStore {
    host: Arc<dyn Sql>,
    dialect: SqlDialect,
}

fn backend(error: StoreError) -> Failure {
    Failure::Backend(error.to_string())
}

fn corrupt(path: &str, why: &'static str) -> Failure {
    Failure::Corrupt(Corrupt {
        path: path.to_owned(),
        why,
    })
}

fn column(row: &SqlRow, at: usize) -> Option<&str> {
    row.get(at).and_then(Option::as_deref)
}

fn number<T: std::str::FromStr>(row: &SqlRow, at: usize, path: &str) -> Result<T, Failure> {
    column(row, at)
        .and_then(|text| text.parse().ok())
        .ok_or_else(|| corrupt(path, "a stored number does not parse"))
}

fn entry_row(path: &str, entry: &Entry) -> Value {
    let text = |value: String| Value::String(value);
    match entry {
        Entry::File {
            digest,
            size,
            mode,
            mtime_ns,
        } => Value::Array(vec![
            text(path.to_owned()),
            text(FILE.to_owned()),
            text(digest.hex()),
            text(size.to_string()),
            text(mode.to_string()),
            text(mtime_ns.to_string()),
            Value::Null,
        ]),
        Entry::Dir { mode, mtime_ns } => Value::Array(vec![
            text(path.to_owned()),
            text(DIR.to_owned()),
            Value::Null,
            text("0".to_owned()),
            text(mode.to_string()),
            text(mtime_ns.to_string()),
            Value::Null,
        ]),
        Entry::Symlink { target, mtime_ns } => Value::Array(vec![
            text(path.to_owned()),
            text(SYMLINK.to_owned()),
            Value::Null,
            text("0".to_owned()),
            text("0".to_owned()),
            text(mtime_ns.to_string()),
            text(target.clone()),
        ]),
    }
}

fn parse_entry(row: &SqlRow) -> Result<Option<(String, Entry)>, Failure> {
    let Some(path) = column(row, 1) else {
        return Ok(None);
    };
    let mtime_ns = number(row, 6, path)?;
    let entry = match column(row, 2) {
        Some(FILE) => Entry::File {
            digest: column(row, 3)
                .and_then(Digest::parse)
                .ok_or_else(|| corrupt(path, "a stored digest does not parse"))?,
            size: number(row, 4, path)?,
            mode: number(row, 5, path)?,
            mtime_ns,
        },
        Some(DIR) => Entry::Dir {
            mode: number(row, 5, path)?,
            mtime_ns,
        },
        Some(SYMLINK) => Entry::Symlink {
            target: column(row, 7)
                .ok_or_else(|| corrupt(path, "a stored symlink has no target"))?
                .to_owned(),
            mtime_ns,
        },
        _ => return Err(corrupt(path, "a stored entry has an unknown kind")),
    };
    Ok(Some((path.to_owned(), entry)))
}

fn json(values: Vec<Value>) -> String {
    Value::Array(values).to_string()
}

impl SqlStore {
    pub async fn open(host: Arc<dyn Sql>) -> Result<Self, Failure> {
        migrate_format(&*host, COMPONENT, &MIGRATIONS)
            .await
            .map_err(backend)?;
        let dialect = host.dialect();
        Ok(Self { host, dialect })
    }

    const fn pick(&self, sql: &Dialects) -> &'static str {
        self.dialect.pick(sql)
    }

    async fn load_manifest(&self, workspace: &str) -> Result<Manifest, Failure> {
        let statements = [SqlStatement {
            sql: self.pick(&LOAD),
            params: vec![workspace],
        }];
        let results = self
            .host
            .run(SqlAccess::Read, "workspace_load", &statements)
            .await
            .map_err(backend)?;
        let rows = results
            .first()
            .ok_or_else(|| Failure::Backend("the SQL host answered no rows set".to_owned()))?;
        let Some(first) = rows.first() else {
            return Ok(Manifest::default());
        };
        let version: u64 = number(first, 0, "")?;
        let mut entries = BTreeMap::new();
        for row in rows {
            if number::<u64>(row, 0, "")? != version {
                return Err(corrupt("", "rows disagree on the head version"));
            }
            if let Some((path, entry)) = parse_entry(row)?
                && entries.insert(path.clone(), entry).is_some()
            {
                return Err(corrupt(&path, "a path is stored twice"));
            }
        }
        Ok(Manifest::from_entries(version, entries)?)
    }

    async fn commit_batch(&self, workspace: &str, batch: &Batch) -> Result<u64, Failure> {
        let change = batch.change();
        let token = nanocode_platform::uuid_v4().to_string();
        let base = change.base.to_string();
        let cleared = json(
            change
                .entries
                .keys()
                .map(|path| Value::String(path.clone()))
                .collect(),
        );
        let put = json(
            change
                .entries
                .iter()
                .filter_map(|(path, entry)| entry.as_ref().map(|entry| entry_row(path, entry)))
                .collect(),
        );
        let dropped = json(
            change
                .dropped
                .iter()
                .map(|digest| Value::String(digest.hex()))
                .collect(),
        );
        let early = mutated(Mutation::SqlReleaseBeforeCommit) && !change.dropped.is_empty();
        let released = change.dropped.iter().filter(|_| !early);
        let mut blobs: Vec<(String, Option<String>)> = batch
            .blobs()
            .iter()
            .map(|blob| (blob.digest().hex(), Some(STANDARD.encode(blob.bytes()))))
            .chain(released.map(|digest| (digest.hex(), None)))
            .collect();
        blobs.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        let lists: Vec<String> = blobs
            .iter()
            .map(|(digest, data)| match data {
                Some(_) => String::new(),
                None => json(vec![Value::String(digest.clone())]),
            })
            .collect();
        let mut statements = Vec::with_capacity(5 + blobs.len());
        statements.push(if change.base == 0 {
            SqlStatement {
                sql: self.pick(&CLAIM_FIRST),
                params: vec![workspace, &token],
            }
        } else {
            SqlStatement {
                sql: self.pick(if mutated(Mutation::SqlSkipVersionCas) {
                    &CLAIM_ANY
                } else {
                    &CLAIM_NEXT
                }),
                params: vec![workspace, &token, &base],
            }
        });
        if !change.entries.is_empty() {
            statements.push(SqlStatement {
                sql: self.pick(&CLEAR),
                params: vec![workspace, &token, &cleared],
            });
            statements.push(SqlStatement {
                sql: self.pick(&PUT),
                params: vec![workspace, &token, &put],
            });
        }
        for ((digest, data), list) in blobs.iter().zip(&lists) {
            statements.push(match data {
                Some(data) => SqlStatement {
                    sql: self.pick(&ADD_BLOB),
                    params: vec![workspace, &token, digest, data],
                },
                None => SqlStatement {
                    sql: self.pick(&RELEASE_BLOBS),
                    params: vec![workspace, &token, list],
                },
            });
        }
        if early {
            let release = [&RELEASE_EARLY, &DROP_EARLY].map(|sql| SqlStatement {
                sql: self.pick(sql),
                params: vec![workspace, &token, &dropped],
            });
            self.host
                .run(SqlAccess::Write, "workspace_release", &release)
                .await
                .map_err(backend)?;
        }
        if !change.dropped.is_empty() && !early {
            statements.push(SqlStatement {
                sql: self.pick(&DROP_BLOBS),
                params: vec![workspace, &token, &dropped],
            });
        }
        statements.push(SqlStatement {
            sql: self.pick(&HEAD),
            params: vec![workspace],
        });
        let results = if mutated(Mutation::SqlSplitTransaction) {
            let mut results = Vec::with_capacity(statements.len());
            for statement in &statements {
                let one = std::slice::from_ref(statement);
                let answered = self
                    .host
                    .run(SqlAccess::Write, "workspace_commit", one)
                    .await
                    .map_err(backend)?;
                results.extend(answered);
            }
            results
        } else {
            self.host
                .transaction("workspace_commit", &statements)
                .await
                .map_err(backend)?
        };
        if results.len() != statements.len() {
            return Err(Failure::Backend(format!(
                "the SQL host answered {} of {} statements",
                results.len(),
                statements.len()
            )));
        }
        let head = results
            .last()
            .and_then(|rows| rows.first())
            .map(|row| -> Result<(u64, bool), Failure> {
                Ok((number(row, 0, "")?, column(row, 1) == Some(token.as_str())))
            })
            .transpose()?;
        match head {
            Some((version, true)) if version == change.version => Ok(version),
            Some((_, true)) => Err(corrupt("", "a claimed head skipped a version")),
            Some((head, false)) => Err(Failure::Refused(Refusal::Stale {
                base: change.base,
                head,
            })),
            None => Err(Failure::Refused(Refusal::Stale {
                base: change.base,
                head: 0,
            })),
        }
    }

    async fn read_blob(&self, digest: &Digest) -> Result<Blob, Failure> {
        let hex = digest.hex();
        let statements = [SqlStatement {
            sql: self.pick(&BLOB),
            params: vec![&hex],
        }];
        let results = self
            .host
            .run(SqlAccess::Read, "workspace_blob", &statements)
            .await
            .map_err(backend)?;
        let Some(data) = results
            .first()
            .and_then(|rows| rows.first())
            .and_then(|row| column(row, 0))
        else {
            return Err(Failure::Missing(*digest));
        };
        let bytes = STANDARD
            .decode(data)
            .map_err(|_| corrupt(&hex, "a stored blob is not base64"))?;
        Blob::verified(*digest, Bytes::from(bytes))
    }
}

impl Store for SqlStore {
    fn load<'a>(&'a self, workspace: &'a str) -> StoreFuture<'a, Result<Manifest, Failure>> {
        Box::pin(self.load_manifest(workspace))
    }

    fn commit<'a>(
        &'a self,
        workspace: &'a str,
        batch: &'a Batch,
    ) -> StoreFuture<'a, Result<u64, Failure>> {
        Box::pin(self.commit_batch(workspace, batch))
    }

    fn blob<'a>(&'a self, digest: &'a Digest) -> StoreFuture<'a, Result<Blob, Failure>> {
        Box::pin(self.read_blob(digest))
    }
}
