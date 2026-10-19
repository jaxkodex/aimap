//! Port of `tests/test_backfill.py`: unknown keys are recorded and the run
//! is idempotent — the in-memory S3 fake plus a real Postgres.

mod support;

use aimap::backfill::backfill;
use aimap::catalog::PgCatalog;
use aimap::config::S3Config;
use aimap::logs::{Capture, Logger};
use aimap::storage::Store;
use support::{MemoryS3, TestDb};

const BUCKET: &str = "aimap-test";

fn email_bytes(n: u32) -> Vec<u8> {
    format!(
        "From: Shop <deals@shop.example>\r\nTo: me@example.com\r\nSubject: m{n}\r\n\
         Date: Thu, 01 Jan 2026 10:00:00 +0000\r\nMessage-ID: <m{n}@example.com>\r\n\
         \r\nbody {n}\r\n"
    )
    .into_bytes()
}

#[test]
fn backfill_records_unknown_keys_and_is_idempotent() {
    let db = TestDb::new();
    let mut conn = db.connect();
    let mem = MemoryS3::new();
    mem.put_with_metadata(
        BUCKET,
        "t/raw/me@x.com/%5BGmail%5D%2FAll%20Mail/3/10.eml",
        &email_bytes(10),
        &[("imap-flags", "\\Seen \\Flagged"), ("imap-internaldate", "01-Jan-2026 10:00:00 +0000")],
    );
    mem.put_plain(BUCKET, "t/raw/me@x.com/INBOX/3/11.eml", &email_bytes(11));
    // same Message-ID as uid 10
    mem.put_plain(BUCKET, "t/raw/me@x.com/INBOX/3/12.eml", &email_bytes(10));
    mem.put_plain(BUCKET, "t/raw/not-a-message.txt", b"x");

    let store = Store::new(
        S3Config { bucket: BUCKET.into(), prefix: "t/".into(), ..Default::default() },
        mem,
    );
    let mut catalog = PgCatalog::new();
    let log = Logger::new(Capture::new(), "INFO", "aimap.backfill");

    let res = backfill(&store, &mut catalog, &mut conn, 2, &log).unwrap();
    assert_eq!(
        (res.scanned, res.recorded, res.new_messages, res.skipped),
        (4, 3, 2, 1)
    );

    let rows = conn
        .query("SELECT mailbox, uid, flags FROM message_locations ORDER BY uid", &[])
        .unwrap();
    let got: Vec<(String, i64, Vec<String>)> = rows
        .iter()
        .map(|r| (r.get::<_, String>(0), r.get::<_, i64>(1), r.get::<_, Vec<String>>(2)))
        .collect();
    assert_eq!(
        got,
        vec![
            ("[Gmail]/All Mail".to_string(), 10, vec!["\\Seen".to_string(), "\\Flagged".to_string()]),
            ("INBOX".to_string(), 11, vec![]),
            ("INBOX".to_string(), 12, vec![]),
        ]
    );
    assert_eq!(conn.query("SELECT count(*) FROM jobs", &[]).unwrap()[0].get::<_, i64>(0), 2);
    assert_eq!(conn.query("SELECT count(*) FROM mailbox_state", &[]).unwrap()[0].get::<_, i64>(0), 0);

    let again = backfill(&store, &mut catalog, &mut conn, 500, &log).unwrap();
    assert_eq!((again.scanned, again.recorded), (4, 0));
}
