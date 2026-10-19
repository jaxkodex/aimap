//! Port of `tests/test_accounts.py` plus the env-account parts of
//! `tests/test_config.py`, against the in-memory S3 fake.

mod support;

use std::cell::RefCell;
use std::collections::HashMap;

use support::MemoryS3;

use aimap::accounts::{
    clean_password, parse_env_accounts, parse_mailboxes_str, Account, AccountRecord, AccountsDoc,
    AccountsRepo, BucketAccounts, EnvAccounts,
};
use aimap::config::S3Config;
use aimap::crypto::{generate_key, Cipher};
use aimap::storage::{S3Error, Store, StoreError};

const BUCKET: &str = "aimap-test";

fn envmap(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
}

struct Fixture {
    store: Store<MemoryS3>,
    cipher: Cipher,
}

fn fixture() -> Fixture {
    let store = Store::new(
        S3Config { bucket: BUCKET.into(), prefix: "t/".into(), ..Default::default() },
        MemoryS3::new(),
    );
    let cipher = Cipher::new(&generate_key()).unwrap();
    Fixture { store, cipher }
}

impl Fixture {
    fn repo(&self) -> AccountsRepo<'_, MemoryS3> {
        AccountsRepo::new(&self.store, "config/accounts.json")
    }
    fn source(&self) -> BucketAccounts<'_, MemoryS3> {
        BucketAccounts::new(self.repo(), self.cipher.clone())
    }
}

fn record(cipher: &Cipher, user: &str) -> AccountRecord {
    AccountRecord {
        user: user.to_string(),
        host: "imap.x.com".to_string(),
        credentials: cipher.encrypt(user, &format!("pw-{user}")),
        ..Default::default()
    }
}

#[test]
fn missing_file_is_empty() {
    let f = fixture();
    let doc = f.repo().read().unwrap();
    assert!(doc.records.is_empty());
    assert_eq!(doc.etag, None);
    assert!(f.source().load().is_empty());
}

#[test]
fn file_has_no_plaintext_password() {
    let f = fixture();
    f.repo()
        .modify(|d| d.records.push(record(&f.cipher, "a@x.com")), 3)
        .unwrap();
    let (body, _) = f.store.get_bytes("t/config/accounts.json").unwrap().unwrap();
    let body = String::from_utf8(body).unwrap();
    assert!(!body.contains("pw-a@x.com"));
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["accounts"][0]["user"], "a@x.com");
}

#[test]
fn source_is_live_and_skips_disabled() {
    let f = fixture();
    let mut src = f.source();
    f.repo().modify(|d| d.records.push(record(&f.cipher, "a")), 3).unwrap();
    assert_eq!(pairs(&mut src), vec![("a".to_string(), "pw-a".to_string())]);
    f.repo()
        .modify(
            |d| {
                let mut r = record(&f.cipher, "b");
                r.enabled = false;
                d.records.push(r);
            },
            3,
        )
        .unwrap();
    assert_eq!(users(&mut src), vec!["a".to_string()]);
    f.repo()
        .modify(
            |d| {
                let r = d.require_mut("b").unwrap();
                r.enabled = true;
                r.touch();
            },
            3,
        )
        .unwrap();
    assert_eq!(users(&mut src), vec!["a".to_string(), "b".to_string()]);
}

fn users<C: aimap::storage::S3Client>(src: &mut BucketAccounts<'_, C>) -> Vec<String> {
    src.load().iter().map(|a| a.user.clone()).collect()
}
fn pairs<C: aimap::storage::S3Client>(src: &mut BucketAccounts<'_, C>) -> Vec<(String, String)> {
    src.load().iter().map(|a| (a.user.clone(), a.password.clone())).collect()
}

/// S3Client wrapper that counts get_object calls (repo.read() only runs when
/// the etag changed).
struct CountingReads<C: aimap::storage::S3Client> {
    inner: C,
    reads: std::sync::atomic::AtomicU32,
}
impl<C: aimap::storage::S3Client> aimap::storage::S3Client for CountingReads<C> {
    fn head_object(&self, b: &str, k: &str) -> Result<Option<String>, S3Error> {
        self.inner.head_object(b, k)
    }
    fn get_object(&self, b: &str, k: &str) -> Result<Option<aimap::storage::S3Object>, S3Error> {
        self.reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.inner.get_object(b, k)
    }
    fn put_object(
        &self,
        b: &str,
        k: &str,
        body: &[u8],
        ct: &str,
        md: &[(String, String)],
        cond: &aimap::storage::PutCondition,
    ) -> Result<String, S3Error> {
        self.inner.put_object(b, k, body, ct, md, cond)
    }
    fn list_objects(&self, b: &str, p: &str) -> Result<Vec<String>, S3Error> {
        self.inner.list_objects(b, p)
    }
    fn head_bucket(&self, b: &str) -> Result<(), S3Error> {
        self.inner.head_bucket(b)
    }
}

#[test]
fn source_only_rereads_when_etag_changes() {
    let counting = CountingReads { reads: std::sync::atomic::AtomicU32::new(0), inner: MemoryS3::new() };
    let store = Store::new(
        S3Config { bucket: BUCKET.into(), prefix: "t/".into(), ..Default::default() },
        counting,
    );
    let cipher = Cipher::new(&generate_key()).unwrap();
    let repo = AccountsRepo::new(&store, "config/accounts.json");
    repo.modify(|d| d.records.push(record(&cipher, "a")), 3).unwrap();
    let mut src = BucketAccounts::new(repo, cipher);
    src.load();
    let reads = |store: &Store<CountingReads<MemoryS3>>| {
        store.client.reads.load(std::sync::atomic::Ordering::SeqCst)
    };
    let after_first = reads(&store);
    src.load();
    src.load();
    assert_eq!(reads(&store), after_first, "unchanged etag must not re-read the file");
    assert_eq!(users(&mut src), vec!["a".to_string()]);
}

#[test]
fn undecryptable_account_skipped_others_kept() {
    let f = fixture();
    let other = Cipher::new(&generate_key()).unwrap();
    f.repo()
        .modify(
            |d| {
                d.records.push(record(&f.cipher, "good"));
                d.records.push(record(&other, "bad"));
            },
            3,
        )
        .unwrap();
    assert_eq!(users(&mut f.source()), vec!["good".to_string()]);
}

#[test]
fn broken_file_keeps_last_good() {
    let f = fixture();
    f.repo().modify(|d| d.records.push(record(&f.cipher, "a")), 3).unwrap();
    let mut src = f.source();
    assert_eq!(src.load().len(), 1);
    f.store.client.put_plain(BUCKET, &f.repo().key, b"{not json");
    assert_eq!(users(&mut src), vec!["a".to_string()]);
}

#[test]
fn conditional_write_detects_concurrent_change() {
    let f = fixture();
    f.repo().modify(|d| d.records.push(record(&f.cipher, "a")), 3).unwrap();
    let stale = f.repo().read().unwrap();
    f.repo().modify(|d| d.records.push(record(&f.cipher, "b")), 3).unwrap();
    let err = f
        .store
        .put_bytes(&f.repo().key, &stale.dump(), "application/json", stale.etag.as_deref())
        .unwrap_err();
    assert!(matches!(err, StoreError::Conflict(_)));
}

#[test]
fn modify_retries_after_conflict() {
    let f = fixture();
    f.repo().modify(|d| d.records.push(record(&f.cipher, "a")), 3).unwrap();
    let repo = f.repo();
    let raced = RefCell::new(false);
    repo.modify(
        |doc| {
            if !*raced.borrow() {
                // another writer sneaks in between our read and write
                *raced.borrow_mut() = true;
                repo.modify(|d| d.records.push(record(&f.cipher, "b")), 3).unwrap();
            }
            doc.records.push(record(&f.cipher, "c"));
        },
        3,
    )
    .unwrap();
    let users: Vec<String> = repo.read().unwrap().records.iter().map(|r| r.user.clone()).collect();
    assert_eq!(users, vec!["a".to_string(), "b".to_string(), "c".to_string()]);
}

#[test]
fn invalid_files() {
    for (body, msg) in [
        (&b"[]"[..], "must be"),
        (&b"{\"version\": 2, \"accounts\": []}"[..], "version"),
        (&b"{\"accounts\": [{\"user\": \"a\"}]}"[..], "invalid account"),
        (
            &b"{\"accounts\": [{\"user\":\"a\",\"host\":\"h\",\"credentials\":\"x\"},{\"user\":\"a\",\"host\":\"h\",\"credentials\":\"x\"}]}"[..],
            "duplicate",
        ),
    ] {
        let err = AccountsDoc::parse(body, Some("etag".to_string())).unwrap_err().0;
        assert!(err.contains(msg), "{body:?}: {err:?} lacks {msg:?}");
    }
}

// --------------------------------------------------------------------------------------
// env accounts (from test_config.py)
// --------------------------------------------------------------------------------------

const ENV: &[(&str, &str)] = &[("IMAP_HOST", "imap.example.com")];

#[test]
fn env_accounts_with_fallbacks() {
    let mut pairs: Vec<(&str, &str)> = ENV.to_vec();
    pairs.extend([
        ("IMAP_USER", "a@x.com"),
        ("IMAP_PASSWORD", "abcd efgh"),
        ("IMAP_USER2", "b@x.com"),
        ("IMAP_PASSWORD2", "p2"),
        ("IMAP_HOST2", "imap.other.com"),
        ("IMAP_MAILBOX2", "INBOX, Archive"),
    ]);
    let accounts = parse_env_accounts(&envmap(&pairs)).unwrap();
    let (a, b) = (&accounts[0], &accounts[1]);
    assert_eq!(
        (a.user.as_str(), a.password.as_str(), a.host.as_str(), a.mailboxes.as_slice()),
        ("a@x.com", "abcdefgh", "imap.example.com", &["INBOX".to_string()][..])
    );
    assert_eq!(
        (b.host.as_str(), b.mailboxes.as_slice()),
        ("imap.other.com", &["INBOX".to_string(), "Archive".to_string()][..])
    );
    assert!(!format!("{a:?}").contains("abcdefgh"));
}

#[test]
fn env_accounts_sorted_numerically() {
    let pairs: Vec<(&str, &str)> = ENV
        .iter()
        .copied()
        .chain([("IMAP_USER10", "u10"), ("IMAP_USER2", "u2"), ("IMAP_PASSWORD10", "p"), ("IMAP_PASSWORD2", "p")])
        .collect();
    let accounts = parse_env_accounts(&envmap(&pairs)).unwrap();
    let users: Vec<&str> = accounts.iter().map(|a| a.user.as_str()).collect();
    assert_eq!(users, vec!["u2", "u10"]);
}

#[test]
fn invalid_env_accounts() {
    // {} -> "not set"
    let e = EnvAccounts::new(&envmap(&[])).unwrap_err().0;
    assert!(e.contains("not set"), "{e}");
    assert!(parse_env_accounts(&envmap(&[("IMAP_USER", "a")])).unwrap_err().0.contains("IMAP_PASSWORD"));
    assert!(parse_env_accounts(&envmap(&[("IMAP_USER", "a"), ("IMAP_PASSWORD", "p")]))
        .unwrap_err()
        .0
        .contains("IMAP_HOST"));
    assert!(parse_env_accounts(&envmap(&[
        ("IMAP_HOST", "imap.example.com"),
        ("IMAP_USER", "a"),
        ("IMAP_PASSWORD", "p"),
        ("IMAP_PORT", "x"),
    ]))
    .unwrap_err()
    .0
    .contains("IMAP_PORT"));
}

#[test]
fn env_source_is_fixed_at_startup() {
    let pairs: Vec<(&str, &str)> = ENV
        .iter()
        .copied()
        .chain([("IMAP_USER", "a"), ("IMAP_PASSWORD", "p w")])
        .collect();
    let mut src = EnvAccounts::new(&envmap(&pairs)).unwrap();
    assert_eq!(src.load(), vec![account("a", "pw")]);
    assert_eq!(src.load(), src.load());
}

fn account(user: &str, password: &str) -> Account {
    Account {
        user: user.to_string(),
        password: password.to_string(),
        host: "imap.example.com".to_string(),
        port: 993,
        mailboxes: vec!["INBOX".to_string()],
    }
}

#[test]
fn helpers() {
    assert_eq!(clean_password("ab cd"), "abcd");
    assert_eq!(clean_password("abcd"), "abcd");
    assert_eq!(
        parse_mailboxes_str("INBOX, Archive , "),
        vec!["INBOX".to_string(), "Archive".to_string()]
    );
    assert_eq!(parse_mailboxes_str(""), vec!["INBOX".to_string()]);
    let v = serde_json::json!(["INBOX", " Archive "]);
    assert_eq!(
        aimap::accounts::parse_mailboxes_value(Some(&v)),
        vec!["INBOX".to_string(), "Archive".to_string()]
    );
    assert_eq!(
        aimap::accounts::parse_mailboxes_value(None),
        vec!["INBOX".to_string()]
    );
}

#[test]
fn dump_parse_round_trip() {
    let f = fixture();
    let rec = record(&f.cipher, "a@x.com");
    let doc = AccountsDoc { records: vec![rec.clone()], etag: Some("\"1\"".to_string()) };
    let bytes = doc.dump();
    let back = AccountsDoc::parse(&bytes, doc.etag.clone()).unwrap();
    assert_eq!(back.records.len(), 1);
    assert_eq!(back.records[0].user, "a@x.com");
    assert_eq!(back.records[0].host, "imap.x.com");
    assert_eq!(back.records[0].port, 993);
    assert!(back.records[0].enabled);
    assert_eq!(back.records[0].credentials, rec.credentials);
    assert_eq!(back.get("a@x.com").unwrap().user, "a@x.com");
    assert!(back.require("missing").is_err());
}

#[test]
fn s3_transport_errors_are_wrapped() {
    // reading through a broken client surfaces as RepoError::Store
    struct Failing;
    impl aimap::storage::S3Client for Failing {
        fn head_object(&self, _: &str, _: &str) -> Result<Option<String>, S3Error> {
            Err(S3Error::new("boom"))
        }
        fn get_object(&self, _: &str, _: &str) -> Result<Option<aimap::storage::S3Object>, S3Error> {
            Err(S3Error::new("boom"))
        }
        fn put_object(
            &self,
            _: &str,
            _: &str,
            _: &[u8],
            _: &str,
            _: &[(String, String)],
            _: &aimap::storage::PutCondition,
        ) -> Result<String, S3Error> {
            Err(S3Error::new("boom"))
        }
        fn list_objects(&self, _: &str, _: &str) -> Result<Vec<String>, S3Error> {
            Err(S3Error::new("boom"))
        }
        fn head_bucket(&self, _: &str) -> Result<(), S3Error> {
            Err(S3Error::new("boom"))
        }
    }
    let store = Store::new(
        S3Config { bucket: "b".into(), prefix: "t/".into(), ..Default::default() },
        Failing,
    );
    let repo = AccountsRepo::new(&store, "config/accounts.json");
    match repo.read() {
        Err(aimap::accounts::RepoError::Store(StoreError::S3(_))) => {}
        other => panic!("expected Store(S3), got {other:?}"),
    }
}

#[test]
fn record_from_dict_defaults_and_errors() {
    let rec = AccountRecord::from_dict(
        &serde_json::from_slice::<serde_json::Map<String, serde_json::Value>>(
            br#"{"user": "a", "host": "h", "credentials": "x"}"#,
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(rec.port, 993);
    assert_eq!(rec.mailboxes, vec!["INBOX".to_string()]);
    assert!(rec.enabled);

    let err = AccountRecord::from_dict(
        &serde_json::from_slice::<serde_json::Map<String, serde_json::Value>>(
            br#"{"user": "a", "host": "h", "credentials": "x", "port": "zz"}"#,
        )
        .unwrap(),
    )
    .unwrap_err();
    assert!(err.0.contains("invalid account record"), "{err}");
    assert!(err.0.contains("zz"), "{err}");
}
