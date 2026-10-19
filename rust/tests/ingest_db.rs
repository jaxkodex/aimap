//! Port of `tests/test_ingest.py`: `sync_mailbox` against an in-memory
//! mailbox fake, the in-memory S3 fake, and a real Postgres.

mod support;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use aimap::catalog::PgCatalog;
use aimap::config::S3Config;
use aimap::ingest::{sync_mailbox, FetchedMessage, MailSource, SyncError, SyncOptions};
use aimap::storage::{S3Client, Store};
use support::{MemoryS3, TestDb};

const BUCKET: &str = "aimap-test";

/// In-memory MailSource: mailboxes: name -> (uidvalidity, {uid: raw}).
type MailboxData = std::collections::BTreeMap<String, (i64, std::collections::BTreeMap<i64, Vec<u8>>)>;

type MailboxSpec<'a> = Vec<(&'a str, i64, Vec<(i64, Vec<u8>)>)>;

struct FakeMailbox {
    mailboxes: Mutex<MailboxData>,
    current: Option<String>,
    fetched: Mutex<Vec<i64>>,
    closed: Arc<AtomicBool>,
    /// uids for which fetch() returns None (the message vanished)
    vanish: Vec<i64>,
    /// set this stop flag during the fetch of the given uid (test hook)
    stop_after_fetch_of: Option<(i64, Arc<AtomicBool>)>,
}

impl FakeMailbox {
    fn new(mailboxes: MailboxSpec<'_>) -> FakeMailbox {
        FakeMailbox {
            mailboxes: Mutex::new(
                mailboxes
                    .into_iter()
                    .map(|(name, uv, msgs)| {
                        (name.to_string(), (uv, msgs.into_iter().collect::<std::collections::BTreeMap<_, _>>()))
                    })
                    .collect(),
            ),
            current: None,
            fetched: Mutex::new(Vec::new()),
            closed: Arc::new(AtomicBool::new(false)),
            vanish: Vec::new(),
            stop_after_fetch_of: None,
        }
    }

    fn set_mailbox(&self, name: &str, uidvalidity: i64, uids: Vec<i64>) {
        let mut boxes = self.mailboxes.lock().unwrap();
        let data = uids.into_iter().map(|u| (u, msg(u as u32))).collect();
        boxes.insert(name.to_string(), (uidvalidity, data));
    }
}

fn msg(n: u32) -> Vec<u8> {
    format!("From: a@example.com\r\nSubject: m{n}\r\n\r\nbody {n}\r\n").into_bytes()
}

impl MailSource for FakeMailbox {
    fn select(&mut self, mailbox: &str) -> Result<i64, SyncError> {
        let boxes = self.mailboxes.lock().unwrap();
        match boxes.get(mailbox) {
            Some((uv, _)) => {
                self.current = Some(mailbox.to_string());
                Ok(*uv)
            }
            None => Err(SyncError::Imap(format!("no mailbox {mailbox}"))),
        }
    }

    fn uids_after(&mut self, last_uid: i64) -> Result<Vec<i64>, SyncError> {
        let boxes = self.mailboxes.lock().unwrap();
        let (_, data) = &boxes[self.current.as_ref().unwrap()];
        Ok(data.keys().copied().filter(|u| *u > last_uid).collect())
    }

    fn fetch(&mut self, uid: i64) -> Result<Option<FetchedMessage>, SyncError> {
        if let Some((at, stop)) = &self.stop_after_fetch_of {
            if *at == uid {
                stop.store(true, Ordering::SeqCst);
            }
        }
        let boxes = self.mailboxes.lock().unwrap();
        let (_, data) = &boxes[self.current.as_ref().unwrap()];
        let Some(raw) = data.get(&uid) else {
            return Ok(None);
        };
        if self.vanish.contains(&uid) {
            return Ok(None);
        }
        self.fetched.lock().unwrap().push(uid);
        Ok(Some(FetchedMessage {
            uid,
            raw: raw.clone(),
            flags: vec!["\\Seen".to_string()],
            internal_date: "01-Jan-2026 10:00:00 +0000".to_string(),
        }))
    }

    fn close(&mut self) {
        self.closed.store(true, Ordering::SeqCst);
    }
}

struct Env {
    store: Store<MemoryS3>,
    catalog: PgCatalog,
    db: TestDb,
}

fn env() -> Env {
    let db = TestDb::new();
    let store = Store::new(
        S3Config { bucket: BUCKET.into(), prefix: "t/".into(), ..Default::default() },
        MemoryS3::new(),
    );
    Env { store, catalog: PgCatalog::new(), db }
}

fn keys(env: &Env, prefix: &str) -> Vec<String> {
    env.store
        .list_raw()
        .unwrap()
        .into_iter()
        .filter(|k| k.starts_with(prefix))
        .collect()
}

fn checkpoint(env: &mut Env, account: &str, mailbox: &str) -> aimap::storage::Checkpoint {
    let mut conn = env.db.connect();
    env.catalog.get_checkpoint(&mut conn, account, mailbox).unwrap().unwrap()
}

fn opts() -> SyncOptions<'static> {
    SyncOptions::default()
}

#[test]
fn first_sync_takes_only_the_most_recent() {
    let mut e = env();
    let mut box_ = FakeMailbox::new(vec![(
        "INBOX",
        7,
        (1..=10).map(|u| (u, msg(u as u32))).collect(),
    )]);
    let mut conn = e.db.connect();
    let r = sync_mailbox(
        &mut box_,
        &e.store,
        &mut e.catalog,
        &mut conn,
        "me@example.com",
        "INBOX",
        &SyncOptions { initial_fetch_count: 3, ..Default::default() },
    )
    .unwrap();
    assert_eq!(r.stored, 3);
    assert_eq!(r.last_uid, 10);
    let mut want: Vec<String> = (8..=10)
        .map(|u| format!("t/raw/me@example.com/INBOX/7/{u}.eml"))
        .collect();
    // S3 listing order is lexicographic (like the Python test's sorted())
    want.sort();
    assert_eq!(keys(&e, "t/raw/"), want);
    let (body, metadata) = e.store.get_raw("t/raw/me@example.com/INBOX/7/10.eml").unwrap().unwrap();
    assert_eq!(body, msg(10));
    let flags = metadata.iter().find(|(k, _)| k == "imap-flags").map(|(_, v)| v.clone()).unwrap();
    assert_eq!(flags, "\\Seen");
}

#[test]
fn incremental_sync_fetches_only_new() {
    let mut e = env();
    let mut box_ = FakeMailbox::new(vec![("INBOX", 7, (1..=3).map(|u| (u, msg(u as u32))).collect())]);
    let mut conn = e.db.connect();
    sync_mailbox(
        &mut box_,
        &e.store,
        &mut e.catalog,
        &mut conn,
        "me",
        "INBOX",
        &SyncOptions { initial_fetch_count: 0, ..Default::default() },
    )
    .unwrap();
    box_.set_mailbox("INBOX", 7, vec![1, 2, 3, 4, 5]);
    box_.fetched.lock().unwrap().clear();
    let r = sync_mailbox(&mut box_, &e.store, &mut e.catalog, &mut conn, "me", "INBOX", &opts()).unwrap();
    assert_eq!(*box_.fetched.lock().unwrap(), vec![4, 5]);
    assert_eq!(r.last_uid, 5);
    let r = sync_mailbox(&mut box_, &e.store, &mut e.catalog, &mut conn, "me", "INBOX", &opts()).unwrap();
    assert_eq!(r.stored, 0);
}

#[test]
fn empty_mailbox_writes_checkpoint() {
    let mut e = env();
    let mut box_ = FakeMailbox::new(vec![("INBOX", 7, vec![])]);
    let mut conn = e.db.connect();
    sync_mailbox(&mut box_, &e.store, &mut e.catalog, &mut conn, "me", "INBOX", &opts()).unwrap();
    let cp = checkpoint(&mut e, "me", "INBOX");
    assert_eq!((cp.uidvalidity, cp.last_uid), (7, 0));
}

#[test]
fn uidvalidity_change_resyncs() {
    let mut e = env();
    let mut box_ = FakeMailbox::new(vec![("INBOX", 7, vec![(1, msg(1)), (2, msg(2))])]);
    let mut conn = e.db.connect();
    sync_mailbox(
        &mut box_,
        &e.store,
        &mut e.catalog,
        &mut conn,
        "me",
        "INBOX",
        &SyncOptions { initial_fetch_count: 0, ..Default::default() },
    )
    .unwrap();
    box_.set_mailbox("INBOX", 8, vec![1]);
    let r = sync_mailbox(
        &mut box_,
        &e.store,
        &mut e.catalog,
        &mut conn,
        "me",
        "INBOX",
        &SyncOptions { initial_fetch_count: 0, ..Default::default() },
    )
    .unwrap();
    assert_eq!((r.stored, r.uidvalidity), (1, 8));
    assert!(keys(&e, "t/raw/").contains(&"t/raw/me/INBOX/8/1.eml".to_string()));
    let cp = checkpoint(&mut e, "me", "INBOX");
    assert_eq!(cp.uidvalidity, 8);
}

#[test]
fn checkpoint_advances_per_batch_and_stop_is_honoured() {
    let mut e = env();
    let mut box_ = FakeMailbox::new(vec![("INBOX", 7, (1..=7).map(|u| (u, msg(u as u32))).collect())]);
    let stop: Arc<AtomicBool> = Arc::new(AtomicBool::new(false));
    box_.stop_after_fetch_of = Some((2, stop.clone()));
    let mut conn = e.db.connect();
    let r = sync_mailbox(
        &mut box_,
        &e.store,
        &mut e.catalog,
        &mut conn,
        "me",
        "INBOX",
        &SyncOptions { initial_fetch_count: 0, batch_size: 3, stop: Some(&stop) },
    )
    .unwrap();
    assert!(r.interrupted);
    assert_eq!(r.last_uid, 3); // first batch finished, then stopped
    assert_eq!(checkpoint(&mut e, "me", "INBOX").last_uid, 3);
}

#[test]
fn vanished_message_is_skipped() {
    let mut e = env();
    let mut box_ = FakeMailbox::new(vec![("INBOX", 7, vec![(1, msg(1)), (2, msg(2))])]);
    box_.vanish.push(1);
    let mut conn = e.db.connect();
    let r = sync_mailbox(
        &mut box_,
        &e.store,
        &mut e.catalog,
        &mut conn,
        "me",
        "INBOX",
        &SyncOptions { initial_fetch_count: 0, ..Default::default() },
    )
    .unwrap();
    assert_eq!((r.stored, r.last_uid), (1, 2));
}

#[test]
fn mailbox_names_with_slashes_are_escaped() {
    let e = env();
    assert_eq!(
        e.store.raw_key("me@x.com", "[Gmail]/All Mail", 1, 5),
        "t/raw/me@x.com/%5BGmail%5D%2FAll%20Mail/1/5.eml"
    );
}

#[test]
fn batches_are_recorded_with_the_checkpoint() {
    let mut e = env();
    let mut box_ = FakeMailbox::new(vec![("INBOX", 7, (1..=5).map(|u| (u, msg(u as u32))).collect())]);
    let mut conn = e.db.connect();
    sync_mailbox(
        &mut box_,
        &e.store,
        &mut e.catalog,
        &mut conn,
        "me",
        "INBOX",
        &SyncOptions { initial_fetch_count: 0, batch_size: 2, ..Default::default() },
    )
    .unwrap();
    // 5 uids / batch size 2 -> three batches, last one advanced to uid 5
    assert_eq!(checkpoint(&mut e, "me", "INBOX").last_uid, 5);
    // message 1's location recorded under its rfc822 message-id
    let sha = {
        use sha2::{Digest, Sha256};
        let digest = Sha256::digest(msg(1));
        digest.iter().map(|b| format!("{b:02x}")).collect::<String>()
    };
    let mut conn = e.db.connect();
    let n: i64 = conn
        .query_one(
            "SELECT count(*) FROM message_locations l
             JOIN messages m ON m.id = l.message_id
             WHERE m.rfc822_message_id = $1 AND l.s3_key = $2",
            &[&format!("sha256:{sha}"), &"t/raw/me/INBOX/7/1.eml".to_string()],
        )
        .unwrap()
        .get(0);
    assert_eq!(n, 1);
}

#[test]
fn legacy_s3_checkpoint_is_imported_once() {
    let mut e = env();
    e.store
        .client
        .put_plain(BUCKET, "t/state/me/INBOX.json", br#"{"uidvalidity": 7, "last_uid": 3}"#);
    let mut box_ = FakeMailbox::new(vec![("INBOX", 7, (1..=5).map(|u| (u, msg(u as u32))).collect())]);
    let mut conn = e.db.connect();
    let r = sync_mailbox(
        &mut box_,
        &e.store,
        &mut e.catalog,
        &mut conn,
        "me",
        "INBOX",
        &SyncOptions { initial_fetch_count: 0, ..Default::default() },
    )
    .unwrap();
    assert_eq!(*box_.fetched.lock().unwrap(), vec![4, 5]);
    assert_eq!(r.last_uid, 5);
    assert_eq!(checkpoint(&mut e, "me", "INBOX").last_uid, 5);
    // the legacy object is left alone, no longer written
    assert!(e.store.client.head_object(BUCKET, "t/state/me/INBOX.json").unwrap().is_some());
}

#[test]
fn select_failure_propagates() {
    let mut e = env();
    let mut box_ = FakeMailbox::new(vec![("INBOX", 7, vec![])]);
    let mut conn = e.db.connect();
    let err = sync_mailbox(&mut box_, &e.store, &mut e.catalog, &mut conn, "me", "Archive", &opts()).unwrap_err();
    assert!(err.to_string().contains("no mailbox Archive"), "{err}");
}
