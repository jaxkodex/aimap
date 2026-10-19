//! `aimap backfill`: record messages that are in S3 but not in Postgres.
//! Port of `backfill.py`: covers mail stored before the database existed and
//! rebuilding the database from the bucket. Lists raw/, skips keys already in
//! message_locations, reads the rest and records them through the same
//! catalog call ingestion uses, which also enqueues their classify jobs.
//! Running it twice changes nothing the second time. Checkpoints are not
//! touched.

use crate::catalog::{PgCatalog, StoredMessage};
use crate::logs::{LogValue, Logger};
use crate::parse::{parse_internal_date, parse_meta};
use crate::storage::{S3Client, S3Error, Store};
use postgres::Client;
use std::collections::BTreeMap;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct BackfillResult {
    pub scanned: usize,
    pub recorded: usize,
    pub new_messages: usize,
    /// keys that do not look like raw/<account>/<mailbox>/<uidvalidity>/<uid>.eml
    pub skipped: usize,
}

#[derive(Debug)]
pub enum BackfillError {
    S3(S3Error),
    Db(postgres::Error),
}

impl std::fmt::Display for BackfillError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BackfillError::S3(e) => write!(f, "{}", e.code),
            BackfillError::Db(e) => write!(f, "{e}"),
        }
    }
}
impl std::error::Error for BackfillError {}

impl From<S3Error> for BackfillError {
    fn from(e: S3Error) -> Self {
        BackfillError::S3(e)
    }
}
impl From<postgres::Error> for BackfillError {
    fn from(e: postgres::Error) -> Self {
        BackfillError::Db(e)
    }
}

fn metadata_get<'m>(meta: &'m [(String, String)], key: &str) -> Option<&'m str> {
    meta.iter().find(|(k, _)| k == key).map(|(_, v)| v.as_str())
}

pub fn backfill<C: S3Client>(
    store: &Store<C>,
    catalog: &mut PgCatalog,
    conn: &mut Client,
    chunk: usize,
    log: &Logger,
) -> Result<BackfillResult, BackfillError> {
    let mut res = BackfillResult::default();
    let keys = store.list_raw()?;
    for batch in keys.chunks(chunk.max(1)) {
        res.scanned += batch.len();
        let known = catalog.known_keys(conn, batch)?;
        // grouped so the assertions (and the catalog writes) are ordered
        let mut groups: BTreeMap<(String, String, i64), Vec<StoredMessage>> = BTreeMap::new();
        for key in batch {
            if known.contains(key) {
                continue;
            }
            let Some(rk) = store.parse_raw_key(key) else {
                res.skipped += 1;
                continue;
            };
            let Some((raw, meta)) = store.get_raw(key)? else {
                continue;
            };
            let flags = metadata_get(&meta, "imap-flags")
                .unwrap_or_default()
                .split_whitespace()
                .map(str::to_string)
                .collect();
            let internal_date =
                parse_internal_date(metadata_get(&meta, "imap-internaldate").unwrap_or_default());
            groups
                .entry((rk.account, rk.mailbox, rk.uidvalidity))
                .or_default()
                .push(StoredMessage {
                    uid: rk.uid,
                    s3_key: key.clone(),
                    meta: parse_meta(&raw),
                    flags,
                    internal_date,
                });
        }
        for ((account, mailbox, uidvalidity), items) in &groups {
            res.new_messages +=
                catalog.record_batch(conn, account, mailbox, *uidvalidity, items, None)? as usize;
            res.recorded += items.len();
        }
        log.info("backfill progress", &[
            ("scanned", LogValue::Int(res.scanned as i64)),
            ("recorded", LogValue::Int(res.recorded as i64)),
        ]);
    }
    Ok(res)
}
