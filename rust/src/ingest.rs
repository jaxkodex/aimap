//! Port of `src/aimap/ingest.py`: copy new messages from one mailbox to S3,
//! record them in Postgres and advance the checkpoint.
//!
//! Order per batch: upload every message to S3, then commit the metadata, the
//! classify jobs and the new checkpoint in one Postgres transaction. A crash
//! before the commit re-uploads the same keys on the next pass and the
//! upserts are no-ops.
//!
//! The mailbox lives behind [`MailSource`] (Python's `MailSource` protocol),
//! implemented by the real IMAP client and by test fakes.

use std::sync::atomic::{AtomicBool, Ordering};

use postgres::Client;

use crate::catalog::{PgCatalog, StoredMessage};
use crate::parse::{parse_internal_date, parse_meta};
use crate::storage::{Checkpoint, Store, StoreError, S3Client};

#[derive(Debug)]
pub enum SyncError {
    Imap(String),
    Store(StoreError),
    Postgres(postgres::Error),
}

impl std::fmt::Display for SyncError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SyncError::Imap(m) => write!(f, "{m}"),
            SyncError::Store(e) => write!(f, "{e}"),
            SyncError::Postgres(e) => write!(f, "{e}"),
        }
    }
}
impl std::error::Error for SyncError {}

impl From<StoreError> for SyncError {
    fn from(e: StoreError) -> Self {
        SyncError::Store(e)
    }
}
impl From<crate::storage::S3Error> for SyncError {
    fn from(e: crate::storage::S3Error) -> Self {
        SyncError::Store(StoreError::S3(e))
    }
}
impl From<postgres::Error> for SyncError {
    fn from(e: postgres::Error) -> Self {
        SyncError::Postgres(e)
    }
}

/// What the ingester needs from a mailbox. Implemented by the IMAP source and
/// by test fakes.
pub trait MailSource {
    /// Open mailbox read-only and return its UIDVALIDITY.
    fn select(&mut self, mailbox: &str) -> Result<i64, SyncError>;
    /// Ascending UIDs greater than last_uid. last_uid=0 means all.
    fn uids_after(&mut self, last_uid: i64) -> Result<Vec<i64>, SyncError>;
    /// Full message, or None if it disappeared since the search.
    fn fetch(&mut self, uid: i64) -> Result<Option<FetchedMessage>, SyncError>;
    fn close(&mut self);
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchedMessage {
    pub uid: i64,
    pub raw: Vec<u8>,
    pub flags: Vec<String>,
    /// raw IMAP internaldate string, e.g. "01-Jan-2026 10:00:00 +0000"
    pub internal_date: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncResult {
    pub account: String,
    pub mailbox: String,
    pub uidvalidity: i64,
    pub stored: usize,
    pub last_uid: i64,
    pub interrupted: bool,
}

pub struct SyncOptions<'a> {
    pub initial_fetch_count: usize,
    pub batch_size: usize,
    /// checked between batches (`threading.Event` in Python)
    pub stop: Option<&'a AtomicBool>,
}

impl Default for SyncOptions<'_> {
    fn default() -> Self {
        SyncOptions { initial_fetch_count: 100, batch_size: 25, stop: None }
    }
}

#[allow(clippy::too_many_arguments)]
pub fn sync_mailbox<S: MailSource, C: S3Client>(
    source: &mut S,
    store: &Store<C>,
    catalog: &mut PgCatalog,
    conn: &mut Client,
    account: &str,
    mailbox: &str,
    options: &SyncOptions<'_>,
) -> Result<SyncResult, SyncError> {
    let uidvalidity = source.select(mailbox)?;
    let mut cp = catalog.get_checkpoint(conn, account, mailbox)?;
    if cp.is_none() {
        if let Some(legacy) = store.legacy_checkpoint(account, mailbox)? {
            catalog.put_checkpoint(conn, account, mailbox, legacy.clone())?;
            cp = Some(legacy);
        }
    }

    if let Some(seen) = &cp {
        if seen.uidvalidity != uidvalidity {
            // The server renumbered the mailbox. Old UIDs mean nothing now;
            // start over under the new UIDVALIDITY. Old objects stay put.
            cp = None;
        }
    }

    let uids: Vec<i64> = match &cp {
        None => {
            let mut all = source.uids_after(0)?;
            if options.initial_fetch_count > 0 && all.len() > options.initial_fetch_count {
                all.drain(..all.len() - options.initial_fetch_count);
            }
            if all.is_empty() {
                catalog.put_checkpoint(conn, account, mailbox, Checkpoint { uidvalidity, last_uid: 0 })?;
            }
            all
        }
        Some(seen) => source.uids_after(seen.last_uid)?,
    };
    // Python keeps `cp` as a local set before the loop and advances it per
    // batch; the tail result reads the last committed one.
    let mut cp = match cp {
        Some(seen) => seen,
        None => Checkpoint { uidvalidity, last_uid: 0 },
    };

    let mut stored = 0usize;
    for start in (0..uids.len()).step_by(options.batch_size.max(1)) {
        if options.stop.is_some_and(|stop| stop.load(Ordering::SeqCst)) {
            return Ok(SyncResult {
                account: account.to_string(),
                mailbox: mailbox.to_string(),
                uidvalidity,
                stored,
                last_uid: cp.last_uid,
                interrupted: true,
            });
        }
        let end = (start + options.batch_size.max(1)).min(uids.len());
        let mut batch: Vec<StoredMessage> = Vec::new();
        for &uid in &uids[start..end] {
            let Some(msg) = source.fetch(uid)? else {
                continue; // message vanished before fetch
            };
            let key = store.put_raw(
                account,
                mailbox,
                uidvalidity,
                uid,
                &msg.raw,
                Some(&[
                    ("imap-flags".to_string(), msg.flags.join(" ")),
                    ("imap-internaldate".to_string(), msg.internal_date.clone()),
                ]),
            )?;
            batch.push(StoredMessage {
                uid,
                s3_key: key,
                meta: parse_meta(&msg.raw),
                flags: msg.flags.clone(),
                internal_date: parse_internal_date(&msg.internal_date),
            });
            stored += 1;
        }
        // Advance only after the whole batch is in S3 and recorded in Postgres.
        cp = Checkpoint { uidvalidity, last_uid: uids[end - 1] };
        catalog.record_batch(conn, account, mailbox, uidvalidity, &batch, Some(cp.clone()))?;
    }

    Ok(SyncResult {
        account: account.to_string(),
        mailbox: mailbox.to_string(),
        uidvalidity,
        stored,
        last_uid: cp.last_uid,
        interrupted: false,
    })
}
