//! Port of `src/aimap/catalog.py`: what ingestion writes to Postgres —
//! accounts, checkpoints, message metadata and jobs.
//!
//! `record_batch` is the hand-off to later stages. In one transaction it
//! upserts the message rows for a batch that is already in S3, enqueues a
//! `classify` job for each new message, and moves the mailbox checkpoint. If
//! the process dies before the commit, the next pass re-uploads the same S3
//! keys and these upserts do nothing the second time.

use std::collections::HashMap;

use postgres::{Client, GenericClient};


use crate::parse::{derive_thread_id, MessageMeta};
use crate::pgtime::Ts;
use crate::storage::Checkpoint;

pub const DEFAULT_PROFILE: &str = "default";
/// enqueued for every new message; "embed" goes here later
pub const STAGES: [&str; 1] = ["classify"];

#[derive(Debug, Clone, PartialEq)]
pub struct StoredMessage {
    pub uid: i64,
    pub s3_key: String,
    pub meta: MessageMeta,
    pub flags: Vec<String>,
    pub internal_date: Option<jiff::Timestamp>,
}

pub struct PgCatalog {
    ids: HashMap<String, i64>,
}

impl Default for PgCatalog {
    fn default() -> Self {
        Self::new()
    }
}

impl PgCatalog {
    pub fn new() -> PgCatalog {
        PgCatalog { ids: HashMap::new() }
    }

    fn account_id(&mut self, conn: &mut impl GenericClient, address: &str) -> Result<i64, postgres::Error> {
        if let Some(id) = self.ids.get(address) {
            return Ok(*id);
        }
        let id: i64 = conn.query_one(
            "INSERT INTO accounts (address, profile_id)
             VALUES ($1, (SELECT id FROM profiles WHERE name = $2))
             ON CONFLICT (address) DO UPDATE SET address = EXCLUDED.address
             RETURNING id",
            &[&address, &DEFAULT_PROFILE],
        )?
        .get(0);
        self.ids.insert(address.to_string(), id);
        Ok(id)
    }

    pub fn account_id_of(&mut self, conn: &mut Client, address: &str) -> Result<i64, postgres::Error> {
        self.account_id(conn, address)
    }

    pub fn sync_accounts(&mut self, conn: &mut Client, addresses: &[String]) -> Result<(), postgres::Error> {
        for a in addresses {
            self.account_id(conn, a)?;
        }
        Ok(())
    }

    pub fn get_checkpoint(&self, conn: &mut impl GenericClient, account: &str, mailbox: &str) -> Result<Option<Checkpoint>, postgres::Error> {
        let row = conn.query_opt(
            "SELECT s.uidvalidity, s.last_uid FROM mailbox_state s
             JOIN accounts a ON a.id = s.account_id
             WHERE a.address = $1 AND s.mailbox = $2",
            &[&account, &mailbox],
        )?;
        Ok(row.map(|r| Checkpoint { uidvalidity: r.get(0), last_uid: r.get(1) }))
    }

    pub fn put_checkpoint(&mut self, conn: &mut Client, account: &str, mailbox: &str, cp: Checkpoint) -> Result<(), postgres::Error> {
        let account_id = self.account_id(conn, account)?;
        put_checkpoint_on(conn, account_id, mailbox, cp)
    }

    /// Store metadata and enqueue jobs; advance the checkpoint if given.
    /// Returns how many messages were new.
    pub fn record_batch(
        &mut self,
        conn: &mut Client,
        account: &str,
        mailbox: &str,
        uidvalidity: i64,
        messages: &[StoredMessage],
        checkpoint: Option<Checkpoint>,
    ) -> Result<i64, postgres::Error> {
        let mut new = 0i64;
        let mut tx = conn.transaction()?;
        let account_id = self.account_id(&mut tx, account)?;
        for sm in messages {
            new += record_one(&mut tx, account_id, mailbox, uidvalidity, sm)?;
        }
        if let Some(cp) = checkpoint {
            put_checkpoint_on(&mut tx, account_id, mailbox, cp)?;
        }
        tx.commit()?;
        Ok(new)
    }

    /// s3_keys that are already stored, from the given list.
    pub fn known_keys(&self, conn: &mut impl GenericClient, keys: &[String]) -> Result<std::collections::HashSet<String>, postgres::Error> {
        Ok(conn
            .query("SELECT s3_key FROM message_locations WHERE s3_key = ANY($1)", &[&keys])?
            .iter()
            .map(|r| r.get::<_, String>(0))
            .collect())
    }
}

pub fn put_checkpoint_on(conn: &mut impl GenericClient, account_id: i64, mailbox: &str, cp: Checkpoint) -> Result<(), postgres::Error> {
    conn.execute(
        "INSERT INTO mailbox_state (account_id, mailbox, uidvalidity, last_uid, updated_at)
         VALUES ($1, $2, $3, $4, now())
         ON CONFLICT (account_id, mailbox) DO UPDATE
         SET uidvalidity = EXCLUDED.uidvalidity, last_uid = EXCLUDED.last_uid, updated_at = now()",
        &[&account_id, &mailbox, &cp.uidvalidity, &cp.last_uid],
    )?;
    Ok(())
}

fn record_one(conn: &mut impl GenericClient, account_id: i64, mailbox: &str, uidvalidity: i64, sm: &StoredMessage) -> Result<i64, postgres::Error> {
    let m = &sm.meta;
    let account_addr: String = conn
        .query_one("SELECT address FROM accounts WHERE id = $1", &[&account_id])?
        .get(0);
    let mut participants = vec![account_addr];
    if let Some(from) = &m.from_email {
        participants.push(from.clone());
    }
    let thread_id = derive_thread_id(
        m.references.as_deref(),
        m.in_reply_to.as_deref(),
        m.subject.as_deref(),
        &participants,
        Some(&m.rfc822_message_id),
    );

    // xmax = 0 means the row was inserted, not updated: a message we had not seen.
    let row = conn.query_one(
        "INSERT INTO messages (account_id, rfc822_message_id, from_email, from_name, subject, sent_at,
                               in_reply_to, message_references, thread_id, has_list_headers)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
         ON CONFLICT (account_id, rfc822_message_id) DO UPDATE SET account_id = EXCLUDED.account_id
         RETURNING id, (xmax = 0)",
        &[
            &account_id,
            &m.rfc822_message_id,
            &m.from_email,
            &m.from_name,
            &m.subject,
            &m.sent_at.as_ref().map(|t| Ts(*t)),
            &m.in_reply_to,
            &m.references,
            &thread_id,
            &m.has_list_headers,
        ],
    )?;
    let message_id: i64 = row.get(0);
    let inserted: bool = row.get(1);

    conn.execute(
        "INSERT INTO message_locations (message_id, account_id, mailbox, uidvalidity, uid, s3_key, flags, internal_date)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
         ON CONFLICT DO NOTHING",
        &[
            &message_id,
            &account_id,
            &mailbox,
            &uidvalidity,
            &sm.uid,
            &sm.s3_key,
            &sm.flags,
            &sm.internal_date.as_ref().map(|t| Ts(*t)),
        ],
    )?;
    for stage in STAGES {
        conn.execute(
            "INSERT INTO jobs (message_id, stage) VALUES ($1, $2) ON CONFLICT DO NOTHING",
            &[&message_id, &stage],
        )?;
    }
    Ok(i64::from(inserted))
}
