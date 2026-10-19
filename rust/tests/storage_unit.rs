//! Tests for `src/storage.rs`. The Python tests drive the real `Store` class
//! over moto; here the same `Store` runs over an in-memory `MemoryS3`, plus
//! quote/unquote parity checks against values produced by
//! `urllib.parse.quote(value, safe="@.-_+")`.

mod support;

use aimap::storage::{
    S3Client,
    ascii_sanitize, seg, unquote, PutCondition, RawKey, S3Config, S3Error, S3Object, Store, StoreError,
};
use support::MemoryS3;

fn store(prefix: &str) -> Store<MemoryS3> {
    Store::new(
        S3Config { bucket: "aimap-test".to_string(), prefix: prefix.to_string(), ..Default::default() },
        MemoryS3::new(),
    )
}

#[test]
fn seg_matches_urllib_parse_quote() {
    // values produced by Python: urllib.parse.quote(v, safe="@.-_+")
    assert_eq!(seg("me@gmail.com"), "me@gmail.com");
    assert_eq!(seg("[Gmail]/All Mail"), "%5BGmail%5D%2FAll%20Mail");
    assert_eq!(seg("INBOX"), "INBOX");
    assert_eq!(seg("a+b@c.d"), "a+b@c.d");
    assert_eq!(seg("Fold/er ä"), "Fold%2Fer%20%C3%A4");
    // round-trip
    for v in ["me@gmail.com", "[Gmail]/All Mail", "Fold/er ä"] {
        assert_eq!(unquote(&seg(v)), v);
    }
}

#[test]
fn raw_and_state_keys_are_segemented() {
    let s = store("t/");
    assert_eq!(
        s.raw_key("me@gmail.com", "[Gmail]/All Mail", 123, 7),
        "t/raw/me@gmail.com/%5BGmail%5D%2FAll%20Mail/123/7.eml"
    );
    assert_eq!(
        s.state_key("me@gmail.com", "[Gmail]/All Mail"),
        "t/state/me@gmail.com/%5BGmail%5D%2FAll%20Mail.json"
    );
    assert_eq!(s.key("/x/y"), "t/x/y");
    assert_eq!(s.key("x/y"), "t/x/y");
}

#[test]
fn put_get_and_etag_flow() {
    let s = store("t/");
    let key = s.put_raw("me@x", "INBOX", 1, 2, b"raw-bytes", Some(&[("imap-flags".into(), "\\\\Seen".into())])).unwrap();
    assert_eq!(key, "t/raw/me@x/INBOX/1/2.eml");

    let (body, md) = s.get_raw(&key).unwrap().unwrap();
    assert_eq!(body, b"raw-bytes");
    assert_eq!(md, vec![("imap-flags".to_string(), "\\\\Seen".to_string())]);

    let etag = s.head_etag(&key).unwrap().unwrap();
    assert!(etag.starts_with("\"etag-"));

    let (body2, got_etag) = s.get_bytes(&key).unwrap().unwrap();
    assert_eq!(body2, b"raw-bytes");
    assert_eq!(got_etag, etag);

    assert!(s.head_etag("t/raw/missing.eml").unwrap().is_none());
    assert!(s.get_raw("t/raw/missing.eml").unwrap().is_none());
    assert!(s.get_bytes("t/missing").unwrap().is_none());
}

#[test]
fn conditional_writes() {
    let s = store("t/");
    let key = "t/data.bin";

    // None expect_etag: only if absent
    let e1 = s.put_bytes(key, b"v1", "application/octet-stream", None).unwrap();
    // second create-conflicts
    assert_eq!(
        s.put_bytes(key, b"again", "application/octet-stream", None),
        Err(StoreError::Conflict(aimap::storage::ConflictError(key.to_string())))
    );
    // wrong etag conflicts
    assert!(matches!(
        s.put_bytes(key, b"v2", "application/octet-stream", Some("\"stale\"")),
        Err(StoreError::Conflict(_))
    ));
    // matching etag overwrites
    let e2 = s.put_bytes(key, b"v2", "application/octet-stream", Some(&e1)).unwrap();
    assert_ne!(e1, e2);
    assert_eq!(s.get_bytes(key).unwrap().unwrap().0, b"v2");
}

#[test]
fn parse_raw_key_round_trip() {
    let s = store("t/");
    let key = s.raw_key("me@gmail.com", "[Gmail]/All Mail", 42, 9);
    assert_eq!(
        s.parse_raw_key(&key),
        Some(RawKey {
            account: "me@gmail.com".to_string(),
            mailbox: "[Gmail]/All Mail".to_string(),
            uidvalidity: 42,
            uid: 9,
        })
    );
    // wrong prefix, wrong shape, wrong extension, non-numeric parts
    assert_eq!(s.parse_raw_key("other/raw/a/b/1/2.eml"), None);
    assert_eq!(s.parse_raw_key("t/raw/a/b/1/2.txt"), None);
    assert_eq!(s.parse_raw_key("t/raw/a/b/1"), None);
    assert_eq!(s.parse_raw_key("t/raw/a/b/notanint/2.eml"), None);
    // the exact production-style key from the docs, under our prefix
    assert_eq!(
        s.parse_raw_key("t/raw/me%40gmail.com/%5BGmail%5D%2FAll%20Mail/111/5.eml"),
        Some(RawKey {
            account: "me@gmail.com".to_string(),
            mailbox: "[Gmail]/All Mail".to_string(),
            uidvalidity: 111,
            uid: 5,
        })
    );
}

#[test]
fn list_raw_walks_the_prefix() {
    let s = store("t/");
    s.put_raw("a", "INBOX", 1, 2, b"x", None).unwrap();
    s.put_raw("a", "INBOX", 1, 1, b"x", None).unwrap();
    s.put_raw("b", "INBOX", 1, 3, b"x", None).unwrap();
    let client = &s.client;
    // put a non-raw key directly
    client.put_object("aimap-test", "t/other.bin", b"x", "application/octet-stream", &[], &PutCondition::None).unwrap();
    let keys = s.list_raw().unwrap();
    assert_eq!(
        keys,
        vec![
            "t/raw/a/INBOX/1/1.eml",
            "t/raw/a/INBOX/1/2.eml",
            "t/raw/b/INBOX/1/3.eml",
        ]
    );
}

#[test]
fn legacy_checkpoints() {
    let s = store("t/");
    assert!(s.legacy_checkpoint("me", "INBOX").unwrap().is_none());

    s.client.put_object(
        "aimap-test",
        &s.state_key("me", "INBOX"),
        br#"{"uidvalidity": 1234, "last_uid": 99}"#,
        "application/json",
        &[],
        &PutCondition::None,
    ).unwrap();
    assert_eq!(
        s.legacy_checkpoint("me", "INBOX").unwrap(),
        Some(aimap::storage::Checkpoint { uidvalidity: 1234, last_uid: 99 })
    );

    // int()-style leniency: numbers as strings
    s.client.put_object(
        "aimap-test",
        &s.state_key("me", "Sent"),
        br#"{"uidvalidity": "77", "last_uid": "12"}"#,
        "application/json",
        &[],
        &PutCondition::None,
    ).unwrap();
    assert_eq!(
        s.legacy_checkpoint("me", "Sent").unwrap(),
        Some(aimap::storage::Checkpoint { uidvalidity: 77, last_uid: 12 })
    );

    // broken payload is an error, not a panic
    s.client.put_object(
        "aimap-test",
        &s.state_key("me", "Drafts"),
        b"not json",
        "application/json",
        &[],
        &PutCondition::None,
    ).unwrap();
    assert!(matches!(
        s.legacy_checkpoint("me", "Drafts"),
        Err(StoreError::Payload(_))
    ));
}

#[test]
fn ascii_metadata_sanitized_like_python() {
    // Python: "ñ".encode("ascii", "backslashreplace") -> b"\\xf1"
    assert_eq!(ascii_sanitize("ñ"), "\\xf1");
    assert_eq!(ascii_sanitize("🔐"), "\\U0001f510");
    assert_eq!(ascii_sanitize("plain"), "plain");
    assert_eq!(ascii_sanitize(&"x".repeat(2000)).chars().count(), 1024);
}

#[test]
fn s3_error_codes_surface() {
    struct Failing;
    impl aimap::storage::S3Client for Failing {
        fn head_object(&self, _: &str, _: &str) -> Result<Option<String>, S3Error> {
            Err(S3Error::new("AccessDenied"))
        }
        fn get_object(&self, _: &str, _: &str) -> Result<Option<S3Object>, S3Error> {
            Err(S3Error::new("AccessDenied"))
        }
        fn put_object(&self, _: &str, _: &str, _: &[u8], _: &str, _: &[(String, String)], _: &PutCondition) -> Result<String, S3Error> {
            Err(S3Error::new("AccessDenied"))
        }
        fn list_objects(&self, _: &str, _: &str) -> Result<Vec<String>, S3Error> {
            Err(S3Error::new("AccessDenied"))
        }
        fn head_bucket(&self, _: &str) -> Result<(), S3Error> {
            Err(S3Error::new("AccessDenied"))
        }
    }
    let s = Store::new(S3Config { bucket: "b".into(), prefix: "t/".into(), ..Default::default() }, Failing);
    assert!(matches!(s.check(), Err(S3Error { code }) if code == "AccessDenied"));
    assert!(matches!(s.get_bytes("t/x"), Err(S3Error { code }) if code == "AccessDenied"));
    assert!(matches!(s.head_etag("t/x"), Err(S3Error { code }) if code == "AccessDenied"));
    assert!(matches!(s.put_raw("a", "INBOX", 1, 1, b"x", None), Err(S3Error { code }) if code == "AccessDenied"));
    assert!(matches!(s.list_raw(), Err(S3Error { code }) if code == "AccessDenied"));
}
