//! Port of `tests/test_catalog.py` (the parts not gated on ingest.py), against
//! a real Postgres. The sync_mailbox-driven tests arrive with the ingest port.

mod support;

use aimap::catalog::{PgCatalog, StoredMessage};
use aimap::parse::parse_meta;
use aimap::storage::Checkpoint;
use support::TestDb;

fn sm(_db: &TestDb, uid: i64, key: &str, n: u32) -> StoredMessage {
    let raw = email_bytes(n);
    let mut meta = parse_meta(&raw);
    meta.sent_at = Some(jiff::Timestamp::from_second(1767225600 + n as i64).unwrap());
    StoredMessage {
        uid,
        s3_key: key.to_string(),
        meta,
        flags: vec!["\\Seen".to_string()],
        internal_date: Some(jiff::Timestamp::from_second(1767225600).unwrap()),
    }
}

fn email_bytes(n: u32) -> Vec<u8> {
    format!(
        "From: Shop <deals@shop.example>\r\nTo: me@example.com\r\nSubject: m{n}\r\n\
         Date: Thu, 01 Jan 2026 10:00:00 +0000\r\nMessage-ID: <m{n}@example.com>\r\n\r\nbody {n}\r\n"
    ).into_bytes()
}

fn rows(db: &TestDb, sql: &str) -> Vec<String> {
    let mut conn = db.connect();
    conn.query(sql, &[])
        .unwrap()
        .iter()
        .map(|r| {
            (0..r.len())
                .map(|i| match r.try_get::<_, Option<String>>(i) {
                    Ok(Some(s)) => s,
                    _ => {
                        let v: Option<i64> = r.try_get(i).ok();
                        v.map(|n| n.to_string()).unwrap_or_default()
                    }
                })
                .collect::<Vec<_>>()
                .join("|")
        })
        .collect()
}

#[test]
fn record_batch_is_idempotent() {
    let db = TestDb::new();
    let mut conn = db.connect();
    let mut cat = PgCatalog::new();
    let sm = sm(&db, 5, "raw/me/INBOX/1/5.eml", 5);
    let cp = Checkpoint { uidvalidity: 1, last_uid: 5 };
    assert_eq!(cat.record_batch(&mut conn, "me", "INBOX", 1, &[sm.clone()][..], Some(cp.clone())).unwrap(), 1);
    assert_eq!(cat.record_batch(&mut conn, "me", "INBOX", 1, &[sm], Some(cp)).unwrap(), 0);
    assert_eq!(rows(&db, "SELECT count(*) FROM message_locations"), vec!["1"]);
}

#[test]
fn same_message_in_two_mailboxes_is_one_message_and_one_job() {
    let db = TestDb::new();
    let mut conn = db.connect();
    let mut cat = PgCatalog::new();
    let raw = email_bytes(1);
    let shared = sm(&db, 1, "raw/me/INBOX/1/1.eml", 1);
    // same rfc822_message_id both times: <m1@example.com>
    let _ = raw;
    cat.record_batch(&mut conn, "me", "INBOX", 7, std::slice::from_ref(&shared), None).unwrap();
    let all_mail = StoredMessage {
        uid: 40,
        s3_key: "raw/me/%5BGmail%5D%2FAll%20Mail/9/40.eml".to_string(),
        ..shared
    };
    cat.record_batch(&mut conn, "me", "[Gmail]/All Mail", 9, &[all_mail], None).unwrap();
    assert_eq!(rows(&db, "SELECT count(*) FROM messages"), vec!["1"]);
    assert_eq!(
        rows(&db, "SELECT mailbox || ':' || uid FROM message_locations ORDER BY id"),
        vec!["INBOX:1".to_string(), "[Gmail]/All Mail:40".to_string()]
    );
    assert_eq!(rows(&db, "SELECT count(*) FROM jobs"), vec!["1"]);
}

#[test]
fn same_message_id_in_two_accounts_is_two_messages() {
    let db = TestDb::new();
    let mut conn = db.connect();
    let mut cat = PgCatalog::new();
    for acct in ["a@x", "b@x"] {
        let raw = email_bytes(1);
        let sm = StoredMessage {
            uid: 1,
            s3_key: format!("raw/{acct}/INBOX/1/1.eml"),
            meta: parse_meta(&raw),
            flags: vec![],
            internal_date: None,
        };
        cat.record_batch(&mut conn, acct, "INBOX", 1, &[sm], None).unwrap();
    }
    assert_eq!(rows(&db, "SELECT count(*) FROM messages"), vec!["2"]);
}

#[test]
fn batch_and_checkpoint_commit_together() {
    let db = TestDb::new();
    let mut conn = db.connect();
    let mut cat = PgCatalog::new();
    let good = sm(&db, 1, "k1", 1);
    // A NUL byte in flags makes the message_locations insert fail server-side
    // (Postgres TEXT cannot hold NUL), which aborts the whole batch — the same
    // property the Python test checks by monkeypatching _record_one.
    let mut bad = sm(&db, 2, "k2", 2);
    bad.flags = vec!["bad\u{0}flag".to_string()];

    let err = cat.record_batch(&mut conn, "me", "INBOX", 1, &[good, bad], Some(Checkpoint { uidvalidity: 1, last_uid: 2 }));
    assert!(err.is_err(), "the bad row must fail the batch");
    assert_eq!(rows(&db, "SELECT count(*) FROM messages"), vec!["0"]);
    assert!(cat.get_checkpoint(&mut conn, "me", "INBOX").unwrap().is_none());
}

#[test]
fn uids_above_int32_fit() {
    let db = TestDb::new();
    let mut conn = db.connect();
    let mut cat = PgCatalog::new();
    cat.put_checkpoint(&mut conn, "me", "INBOX", Checkpoint { uidvalidity: 4_000_000_000, last_uid: 4_294_967_295 }).unwrap();
    assert_eq!(
        cat.get_checkpoint(&mut conn, "me", "INBOX").unwrap(),
        Some(Checkpoint { uidvalidity: 4_000_000_000, last_uid: 4_294_967_295 })
    );
}

#[test]
fn known_keys_and_account_ids() {
    let db = TestDb::new();
    let mut conn = db.connect();
    let mut cat = PgCatalog::new();
    let sm = sm(&db, 5, "raw/me/INBOX/1/5.eml", 5);
    cat.record_batch(&mut conn, "me", "INBOX", 1, &[sm], None).unwrap();
    let known = cat.known_keys(&mut conn, &["raw/me/INBOX/1/5.eml".to_string(), "nope".to_string()]).unwrap();
    assert_eq!(known, ["raw/me/INBOX/1/5.eml".to_string()].into_iter().collect());
    // account upsert: same address, same row
    let id1 = cat.account_id_of(&mut conn, "me").unwrap();
    let id2 = cat.account_id_of(&mut conn, "me").unwrap();
    assert_eq!(id1, id2);
}
