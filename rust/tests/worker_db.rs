//! Port of `tests/test_worker.py`: the worker's account refresh, per-account
//! backoff, live add/remove/change handling and clean shutdown — against an
//! in-memory mailbox fake, the in-memory S3 fake and a real Postgres.

mod support;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use aimap::accounts::{Account, AccountSource};
use aimap::catalog::PgCatalog;
use aimap::config::{Settings, S3Config};
use aimap::ingest::{FetchedMessage, MailSource, SyncError};
use aimap::logs::{Capture, Logger};
use aimap::storage::Store;
use aimap::worker::Worker;
use support::{MemoryS3, TestDb};

const BUCKET: &str = "aimap-test";

/// In-memory MailSource: mailboxes: name -> (uidvalidity, {uid: raw}).
type MailboxData = std::collections::BTreeMap<String, (i64, std::collections::BTreeMap<i64, Vec<u8>>)>;

struct FakeMailbox {
    mailboxes: Mutex<MailboxData>,
    current: Option<String>,
    closed: Arc<AtomicBool>,
}

type MailboxSpec<'a> = Vec<(&'a str, i64, Vec<(i64, Vec<u8>)>)>;

fn msg(n: u32) -> Vec<u8> {
    format!("From: a@example.com\r\nSubject: m{n}\r\n\r\nbody {n}\r\n").into_bytes()
}

impl FakeMailbox {
    fn new(mailboxes: MailboxSpec<'_>, closed: Arc<AtomicBool>) -> FakeMailbox {
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
            closed,
        }
    }
}

impl MailSource for FakeMailbox {
    fn select(&mut self, mailbox: &str) -> Result<i64, SyncError> {
        let boxes = self.mailboxes.lock().unwrap();
        match boxes.get(mailbox) {
            Some((uidvalidity, _)) => {
                self.current = Some(mailbox.to_string());
                Ok(*uidvalidity)
            }
            None => Err(SyncError::Imap(format!("cannot open mailbox '{mailbox}'"))),
        }
    }

    fn uids_after(&mut self, last_uid: i64) -> Result<Vec<i64>, SyncError> {
        let boxes = self.mailboxes.lock().unwrap();
        let Some(current) = &self.current else {
            return Err(SyncError::Imap("no mailbox selected".into()));
        };
        let Some((_, msgs)) = boxes.get(current) else {
            return Err(SyncError::Imap("no mailbox selected".into()));
        };
        Ok(msgs.keys().copied().filter(|u| *u > last_uid).collect())
    }

    fn fetch(&mut self, uid: i64) -> Result<Option<FetchedMessage>, SyncError> {
        let boxes = self.mailboxes.lock().unwrap();
        let Some(current) = &self.current else {
            return Err(SyncError::Imap("no mailbox selected".into()));
        };
        let Some((_, msgs)) = boxes.get(current) else {
            return Err(SyncError::Imap("no mailbox selected".into()));
        };
        Ok(msgs.get(&uid).map(|raw| FetchedMessage {
            uid,
            raw: raw.clone(),
            flags: vec![],
            internal_date: String::new(),
        }))
    }

    fn close(&mut self) {
        self.closed.store(true, Ordering::SeqCst);
    }
}

/// `ListSource` in Python: the current account list, changeable mid-test.
/// Clones share the list, so the worker can hold one while the test mutates
/// another.
#[derive(Clone)]
struct ListSource(Arc<Mutex<Vec<Account>>>);

impl ListSource {
    fn new(accounts: Vec<Account>) -> ListSource {
        ListSource(Arc::new(Mutex::new(accounts)))
    }

    fn set(&self, accounts: Vec<Account>) {
        *self.0.lock().unwrap() = accounts;
    }
}

impl AccountSource for ListSource {
    fn load(&mut self) -> Vec<Account> {
        self.0.lock().unwrap().clone()
    }
}

fn account(user: &str, password: &str) -> Account {
    Account { user: user.into(), password: password.into(), host: "h".into(), port: 993, mailboxes: vec!["INBOX".into()] }
}

fn settings() -> Settings {
    Settings { s3: S3Config { bucket: BUCKET.into(), ..Default::default() }, poll_interval: 10.0, max_backoff: 60.0, ..Default::default() }
}

/// The clock: a cell the test moves forward.
#[derive(Clone)]
struct Clock(Arc<std::sync::Mutex<f64>>);

impl Clock {
    fn new() -> Clock {
        Clock(Arc::new(std::sync::Mutex::new(0.0)))
    }

    fn set(&self, t: f64) {
        *self.0.lock().unwrap() = t;
    }
}

#[test]
fn failing_account_backs_off_without_blocking_others() {
    let db = TestDb::new();
    let mut conn = db.connect();
    let store = Store::new(S3Config { bucket: BUCKET.into(), ..Default::default() }, MemoryS3::new());
    let mut catalog = PgCatalog::new();

    let accounts = ListSource::new(vec![account("bad", "p"), account("good", "p")]);
    let now = Clock::new();
    let clock = now.clone();
    let good_closed = Arc::new(AtomicBool::new(false));
    let calls: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let capture = Capture::new();

    let good_closed2 = good_closed.clone();
    let calls2 = calls.clone();
    let mut worker_accounts = accounts.clone();
    let mut w = Worker::new(
        settings(),
        &store,
        &mut catalog,
        &mut worker_accounts,
        Logger::new(capture, "INFO", "aimap.worker"),
    );
    w.source_factory = Some(Box::new(move |acct: &Account| {
        calls2.lock().unwrap().push(acct.user.clone());
        if acct.user == "bad" {
            Err("connection refused".into())
        } else {
            Ok(Box::new(FakeMailbox::new(
                vec![("INBOX", 1, vec![(1, msg(1))])],
                good_closed2.clone(),
            )) as Box<dyn MailSource>)
        }
    }));
    w.set_clock(Box::new(move || *clock.0.lock().unwrap()));

    assert_eq!(w.run_once(&mut conn), 1);
    assert_eq!(w.failed_accounts, vec!["bad".to_string()]);
    assert!(good_closed.load(Ordering::SeqCst));

    // backoff 10s, then 20s after the second failure
    for (t, expected) in [
        (5.0, vec!["good"]),
        (11.0, vec!["bad", "good"]),
        (25.0, vec!["good"]),
        (32.0, vec!["bad", "good"]),
    ] {
        now.set(t);
        calls.lock().unwrap().clear();
        w.run_once(&mut conn);
        assert_eq!(*calls.lock().unwrap(), expected.iter().map(|s| s.to_string()).collect::<Vec<_>>(), "t={t}");
    }
}

#[test]
fn accounts_added_and_removed_live() {
    let db = TestDb::new();
    let mut conn = db.connect();
    let store = Store::new(S3Config { bucket: BUCKET.into(), ..Default::default() }, MemoryS3::new());
    let mut catalog = PgCatalog::new();

    let accounts = ListSource::new(vec![]);
    let capture = Capture::new();
    let good_closed = Arc::new(AtomicBool::new(false));
    let mut worker_accounts = accounts.clone();
    let mut w = Worker::new(
        settings(),
        &store,
        &mut catalog,
        &mut worker_accounts,
        Logger::new(capture, "INFO", "aimap.worker"),
    );
    w.source_factory = Some(Box::new(move |_acct: &Account| {
        Ok(Box::new(FakeMailbox::new(vec![("INBOX", 1, vec![(1, msg(1))])], good_closed.clone())) as Box<dyn MailSource>)
    }));

    assert_eq!(w.run_once(&mut conn), 0);
    accounts.set(vec![account("new", "p")]);
    assert_eq!(w.run_once(&mut conn), 1);
    assert_eq!(conn.query("SELECT address FROM accounts", &[]).unwrap().len(), 1);
    accounts.set(vec![]);
    w.run_once(&mut conn);
    assert_eq!(w.tracked_users(), Vec::<String>::new());
}

#[test]
fn changed_account_resets_backoff() {
    let db = TestDb::new();
    let mut conn = db.connect();
    let store = Store::new(S3Config { bucket: BUCKET.into(), ..Default::default() }, MemoryS3::new());
    let mut catalog = PgCatalog::new();

    let accounts = ListSource::new(vec![account("u", "wrong")]);
    let now = Clock::new();
    let clock = now.clone();
    let capture = Capture::new();
    let good_closed = Arc::new(AtomicBool::new(false));
    let mut worker_accounts = accounts.clone();
    let mut w = Worker::new(
        settings(),
        &store,
        &mut catalog,
        &mut worker_accounts,
        Logger::new(capture, "INFO", "aimap.worker"),
    );
    w.source_factory = Some(Box::new(move |acct: &Account| {
        if acct.password == "wrong" {
            Err("auth failed".into())
        } else {
            Ok(Box::new(FakeMailbox::new(vec![("INBOX", 1, vec![(1, msg(1))])], good_closed.clone())) as Box<dyn MailSource>)
        }
    }));
    w.set_clock(Box::new(move || *clock.0.lock().unwrap()));

    w.run_once(&mut conn);
    assert_eq!(w.failed_accounts, vec!["u".to_string()]);
    // fixed via set-password
    accounts.set(vec![account("u", "right")]);
    now.set(1.0); // still inside the backoff window, but the account changed
    assert_eq!(w.run_once(&mut conn), 1);
}

#[test]
fn empty_warning_logged_once() {
    let db = TestDb::new();
    let mut conn = db.connect();
    let store = Store::new(S3Config { bucket: BUCKET.into(), ..Default::default() }, MemoryS3::new());
    let mut catalog = PgCatalog::new();

    let accounts = ListSource::new(vec![]);
    let capture = Capture::new();
    let mut worker_accounts = accounts.clone();
    let mut w = Worker::new(
        settings(),
        &store,
        &mut catalog,
        &mut worker_accounts,
        Logger::new(capture.clone(), "INFO", "aimap.worker"),
    );
    w.source_factory = Some(Box::new(|_acct: &Account| Err("not reached".into())));

    w.run_once(&mut conn);
    w.run_once(&mut conn);
    assert_eq!(capture.count_containing("no accounts"), 1);
}

#[test]
fn run_forever_exits_when_stopped() {
    let db = TestDb::new();
    let mut conn = db.connect();
    let store = Store::new(S3Config { bucket: BUCKET.into(), ..Default::default() }, MemoryS3::new());
    let mut catalog = PgCatalog::new();

    let accounts = ListSource::new(vec![]);
    let capture = Capture::new();
    let mut worker_accounts = accounts.clone();
    let mut w = Worker::new(
        settings(),
        &store,
        &mut catalog,
        &mut worker_accounts,
        Logger::new(capture, "INFO", "aimap.worker"),
    );
    w.stop.set();
    w.run_forever(&mut conn);
}
