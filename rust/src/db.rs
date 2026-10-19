//! Port of `src/aimap/db.py`: schema migrations.
//!
//! Migrations are numbered SQL files in `src/aimap/migrations` (0001_init.sql,
//! 0002_...). `migrate` applies the missing ones in order, each in its own
//! transaction, under an advisory lock so two deploys cannot run them at once.
//! Applied versions are recorded in schema_migrations. The SQL files are
//! embedded into the binary via `include_dir!` straight from the Python
//! package, so both runtimes share exactly one schema source.

use include_dir::{include_dir, Dir};
use postgres::{Client, GenericClient};
use regex::Regex;
use std::sync::LazyLock;

/// "aimap"
const LOCK_ID: i64 = 0x61696D6170;

static MIGRATIONS_DIR: Dir = include_dir!("$CARGO_MANIFEST_DIR/../src/aimap/migrations");

static FILE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(\d{4})_[\w-]+\.sql$").unwrap());

/// `SchemaError` in db.py.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaError(pub String);

impl std::fmt::Display for SchemaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::error::Error for SchemaError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Migration {
    pub version: i32,
    pub name: String,
    pub sql: String,
}

pub fn migrations() -> Result<Vec<Migration>, SchemaError> {
    let mut found = Vec::new();
    for entry in MIGRATIONS_DIR.files() {
        let name = entry.path().file_name().and_then(|n| n.to_str()).unwrap_or("");
        if let Some(caps) = FILE_RE.captures(name) {
            found.push(Migration {
                version: caps[1].parse().unwrap(),
                name: name.to_string(),
                sql: String::from_utf8_lossy(entry.contents()).into_owned(),
            });
        }
    }
    found.sort_by_key(|m| m.version);
    let versions: Vec<i32> = found.iter().map(|m| m.version).collect();
    let mut unique = versions.clone();
    unique.sort_unstable();
    unique.dedup();
    if unique.len() != versions.len() {
        return Err(SchemaError(format!("duplicate migration versions: {versions:?}")));
    }
    Ok(found)
}

pub fn applied_versions(conn: &mut impl GenericClient) -> Result<std::collections::HashSet<i32>, postgres::Error> {
    let exists: bool = conn
        .query_one("SELECT to_regclass('schema_migrations') IS NOT NULL", &[])?
        .get(0);
    if !exists {
        return Ok(Default::default());
    }
    Ok(conn
        .query("SELECT version FROM schema_migrations", &[])?
        .iter()
        .map(|row| row.get(0))
        .collect())
}

pub fn pending(conn: &mut impl GenericClient) -> Result<Vec<Migration>, DbError> {
    let done = applied_versions(conn)?;
    let mut list: Vec<Migration> = migrations()?
        .into_iter()
        .filter(|m| !done.contains(&m.version))
        .collect();
    list.sort_by_key(|m| m.version);
    Ok(list)
}

/// Apply missing migrations. Returns the names of the ones applied.
pub fn migrate(dsn: &str) -> Result<Vec<String>, DbError> {
    let mut client = Client::connect(dsn, postgres::NoTls)?;
    migrate_on(&mut client)
}

/// Same as [`migrate`] over an existing connection (tests, embedding).
pub fn migrate_on(client: &mut Client) -> Result<Vec<String>, DbError> {
    let mut applied = Vec::new();
    // Session-level advisory lock; the connection stays in autocommit so each
    // migration commits on its own, exactly like db.migrate in Python.
    client.execute("SELECT pg_advisory_lock($1)", &[&LOCK_ID])?;
    ensure_table(client)?;
    for m in pending(client)? {
        // re-read under the lock; simple_query allows the multi-statement SQL files
        client.simple_query("BEGIN")?;
        let result = client
            .simple_query(&m.sql)
            .and_then(|_| {
                client.execute(
                    "INSERT INTO schema_migrations (version, name) VALUES ($1, $2)",
                    &[&m.version, &m.name],
                )
            });
        match result {
            Ok(_) => {
                client.simple_query("COMMIT")?;
            }
            Err(e) => {
                client.simple_query("ROLLBACK")?;
                return Err(e.into());
            }
        }
        applied.push(m.name);
    }
    client.execute("SELECT pg_advisory_unlock($1)", &[&LOCK_ID])?;
    Ok(applied)
}

fn ensure_table(client: &mut Client) -> Result<(), postgres::Error> {
    client.execute(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
            version     INT PRIMARY KEY,
            name        TEXT NOT NULL,
            applied_at  TIMESTAMPTZ NOT NULL DEFAULT now()
        )",
        &[],
    )?;
    Ok(())
}

/// `require_current`: refuse to start if any migration is missing.
pub fn require_current(conn: &mut impl GenericClient) -> Result<(), SchemaError> {
    let missing = pending(conn).map_err(|e| SchemaError(e.to_string()))?;
    if !missing.is_empty() {
        let names: Vec<String> = missing.iter().map(|m| m.name.clone()).collect();
        return Err(SchemaError(format!(
            "database schema is behind, run `aimap migrate` (missing: {})",
            names.join(", ")
        )));
    }
    Ok(())
}

/// Wraps migration or SQL errors, like the RuntimeError family in db.py.
#[derive(Debug)]
pub enum DbError {
    Schema(SchemaError),
    Postgres(postgres::Error),
}

impl std::fmt::Display for DbError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DbError::Schema(e) => write!(f, "{e}"),
            DbError::Postgres(e) => write!(f, "{e}"),
        }
    }
}
impl std::error::Error for DbError {}

impl From<SchemaError> for DbError {
    fn from(e: SchemaError) -> Self {
        DbError::Schema(e)
    }
}
impl From<postgres::Error> for DbError {
    fn from(e: postgres::Error) -> Self {
        DbError::Postgres(e)
    }
}
