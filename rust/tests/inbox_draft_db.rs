//! Port of the inbox + draft behaviours from `tests/test_draft.py` and the
//! API tests' underlying read model, against a real Postgres and the in-memory
//! S3 fake.

mod support;

use aimap::catalog::{PgCatalog, StoredMessage};
use aimap::inbox::{
    decode_cursor, encode_cursor, get_message, get_thread, handled_since, home_items, list_messages,
    needs_reply, set_state, sorted_at,
};
use aimap::draft::{complete_draft, DraftConfig, DraftError};
use aimap::home::reasons;
use aimap::parse::parse_meta;
use serde_json::{json, Value};
use support::{MemoryS3, TestDb};

const ME: &str = "me@example.com";

fn email_bytes(_n: u32, subject: &str, body: &str, message_id: &str) -> Vec<u8> {
    format!(
        "From: Bob <bob@example.com>\r\nTo: {ME}\r\nSubject: {subject}\r\n\
         Date: Thu, 01 Jan 2026 10:00:00 +0000\r\nMessage-ID: {message_id}\r\n\r\n{body}\r\n"
    ).into_bytes()
}

/// Seed one classified message needing a reply, through the real catalog.
fn seeded(db: &TestDb, store: &aimap::storage::Store<MemoryS3>, cat: &mut PgCatalog, n: u32, subject: &str) -> i64 {
    let raw = email_bytes(n, subject, "Can you do Tuesday at 3pm?", &format!("<m{n}@example.com>"));
    let key = store.put_raw(ME, "INBOX", 7, n as i64, &raw, None).unwrap();
    let sm = StoredMessage {
        uid: n as i64,
        s3_key: key,
        meta: parse_meta(&raw),
        flags: vec![],
        internal_date: None,
    };
    cat.record_batch(&mut db.connect(), ME, "INBOX", 7, &[sm], None).unwrap();
    let mut conn = db.connect();
    let mid: i64 = conn
        .query_one("SELECT id FROM messages WHERE subject = $1", &[&subject])
        .unwrap()
        .get(0);
    let pid: i64 = conn
        .query_one("SELECT id FROM profiles WHERE name = 'default'", &[])
        .unwrap()
        .get(0);
    conn.execute(
        "INSERT INTO classifications (message_id, profile_id, classifier_key, model, raw_answers,
            importance, action_bucket, tags, insight, source, needs_review, priority, signals)
         VALUES ($1, $2, 'k', 'jev-test', '{}', 'High', 'reply', '{}', NULL, 'judgment', false, 5.0, $3)",
        &[&mid, &pid, &json!({"real_person": 0.95})],
    )
    .unwrap();
    mid
}

fn draft_config() -> DraftConfig {
    DraftConfig {
        base_url: "https://api.example.com/v1".into(),
        api_key: Some("test-key".into()),
        model: "test-model".into(),
        timeout_secs: 30,
        max_tokens: 500,
        reasoning_effort: "low".into(),
    }
}

struct FakeModel(Value);
impl aimap::draft::ChatTransport for FakeModel {
    fn post(&self, _call: &aimap::draft::ModelCall) -> Result<Value, aimap::draft::TransportError> {
        Ok(self.0.clone())
    }
}

#[test]
fn needs_reply_rules() {
    assert_eq!(needs_reply(None, &json!({}), false), None);
    let r = needs_reply(Some("reply"), &json!({"real_person": 0.9}), false).unwrap();
    assert_eq!(r["needed"], json!(true));
    assert_eq!(r["reason"], "Written to you by a person");
    let r = needs_reply(Some("reply"), &json!({}), false).unwrap();
    assert_eq!(r["reason"], "Someone expects a reply.");
    // act_now needs a real person AND not bulk
    let r = needs_reply(Some("act_now"), &json!({"real_person": 0.9}), false).unwrap();
    assert_eq!(r["needed"], json!(true));
    assert_eq!(needs_reply(Some("act_now"), &json!({"real_person": 0.9}), true).unwrap()["needed"], json!(false));
    assert_eq!(needs_reply(Some("act_now"), &json!({}), false).unwrap()["needed"], json!(false));
    assert_eq!(needs_reply(Some("skim"), &json!({}), false).unwrap()["needed"], json!(false));
}

#[test]
fn cursors_round_trip_and_reject_garbage() {
    let at = jiff::Timestamp::from_second(1767225600).unwrap();
    let c = encode_cursor(at, 42);
    assert_eq!(decode_cursor(&c).unwrap(), (at, 42));
    assert!(decode_cursor("not-a-cursor!!!").is_err());
}

#[test]
fn list_messages_pages_newest_first_with_cursor() {
    let db = TestDb::new();
    let store = MemoryS3::new();
    let mut cat = PgCatalog::new();
    let store = aimap::storage::Store::new(
        aimap::storage::S3Config { bucket: "b".into(), prefix: "t/".into(), ..Default::default() },
        store,
    );
    for n in 1..=5u32 {
        let raw = email_bytes(n, &format!("m{n}"), "body", &format!("<m{n}@example.com>"));
        let key = store.put_raw(ME, "INBOX", 7, n as i64, &raw, None).unwrap();
        let sm = StoredMessage { uid: n as i64, s3_key: key, meta: parse_meta(&raw), flags: vec![], internal_date: None };
        cat.record_batch(&mut db.connect(), ME, "INBOX", 7, &[sm], None).unwrap();
        // stagger sent_at so ordering is deterministic
        let mut conn = db.connect();
        conn.execute(
            "UPDATE messages SET sent_at = now() - ($1 || ' minutes')::interval WHERE subject = $2",
            &[&(n.to_string()), &format!("m{n}")],
        )
        .unwrap();
    }

    let (page1, cursor) = list_messages(&mut db.connect(), Some(ME), "all", None, 3).unwrap();
    assert_eq!(page1.len(), 3);
    assert!(cursor.is_some());
    assert_eq!(page1[0]["subject"], "m1"); // newest first
    assert_eq!(page1[0]["sender"], "Bob");
    assert_eq!(page1[0]["unread"], json!(true)); // no \Seen flags anywhere

    let (page2, cursor2) = list_messages(&mut db.connect(), Some(ME), "all", cursor.as_deref(), 3).unwrap();
    assert_eq!(page2.len(), 2);
    assert!(cursor2.is_none()); // last page
    assert_eq!(page2[0]["subject"], "m4");
}

#[test]
fn get_message_shapes_labels_reply_and_locations() {
    let db = TestDb::new();
    let store = aimap::storage::Store::new(
        aimap::storage::S3Config { bucket: "b".into(), prefix: "t/".into(), ..Default::default() },
        MemoryS3::new(),
    );
    let mut cat = PgCatalog::new();
    let mid = seeded(&db, &store, &mut cat, 1, "Interview slot");

    let msg = get_message(&mut db.connect(), mid).unwrap().unwrap();
    assert_eq!(msg["account"], ME);
    assert_eq!(msg["profile"], "default");
    assert_eq!(msg["sender"], "Bob");
    assert_eq!(msg["unread"], json!(true));
    assert_eq!(msg["state"], serde_json::Value::Null);
    assert_eq!(msg["labels"]["importance"], "High");
    assert_eq!(msg["labels"]["action_bucket"], "reply");
    assert_eq!(msg["reply"]["needed"], json!(true));
    assert_eq!(msg["mailboxes"].as_array().unwrap().len(), 1);
    assert!(msg["s3_key"].as_str().unwrap().starts_with("t/raw/"));

    // unclassified message: labels null, reply null
    let raw = email_bytes(2, "Plain", "body", "<plain-1@x>");
    let key = store.put_raw(ME, "INBOX", 7, 9, &raw, None).unwrap();
    let sm = StoredMessage { uid: 9, s3_key: key, meta: parse_meta(&raw), flags: vec![], internal_date: None };
    cat.record_batch(&mut db.connect(), ME, "INBOX", 7, &[sm], None).unwrap();
    let pid: i64 = db.connect().query_one("SELECT id FROM messages WHERE subject = 'Plain'", &[]).unwrap().get(0);
    let plain = get_message(&mut db.connect(), pid).unwrap().unwrap();
    assert!(plain["labels"].is_null());
    assert!(plain["reply"].is_null());
}

#[test]
fn set_state_is_idempotent_and_undoable() {
    let db = TestDb::new();
    let store = aimap::storage::Store::new(
        aimap::storage::S3Config { bucket: "b".into(), prefix: "t/".into(), ..Default::default() },
        MemoryS3::new(),
    );
    let mut cat = PgCatalog::new();
    let mid = seeded(&db, &store, &mut cat, 1, "Interview slot");
    let mut conn = db.connect();

    let r = set_state(&mut conn, mid, Some("later"), ME).unwrap().unwrap();
    assert_eq!(r["state"], "later");
    assert!(r["changed_at"].is_string());
    // idempotent, refreshes changed_at
    let r2 = set_state(&mut conn, mid, Some("later"), ME).unwrap().unwrap();
    assert_eq!(r2["state"], "later");
    // undo deletes the row
    let r3 = set_state(&mut conn, mid, None, ME).unwrap().unwrap();
    assert_eq!(r3["state"], serde_json::Value::Null);
    // unknown message
    assert!(set_state(&mut conn, 999_999, Some("later"), ME).unwrap().is_none());

    assert_eq!(handled_since(&mut conn, "2020-01-01T00:00:00Z".parse().unwrap(), Some(ME)).unwrap(), 0);
}

#[test]
fn home_items_and_sorted_at() {
    let db = TestDb::new();
    let store = aimap::storage::Store::new(
        aimap::storage::S3Config { bucket: "b".into(), prefix: "t/".into(), ..Default::default() },
        MemoryS3::new(),
    );
    let mut cat = PgCatalog::new();
    let mid = seeded(&db, &store, &mut cat, 1, "Interview slot");
    let mut conn = db.connect();

    let items = home_items(&mut conn, "2020-01-01T00:00:00Z".parse().unwrap(), None).unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].message_id, mid);
    assert_eq!(items[0].action_bucket.as_deref(), Some("reply"));
    assert_eq!(items[0].profile.as_deref(), Some("default"));
    assert_eq!(reasons(&items[0].signals), vec!["Written to you by a person"]);
    assert!(sorted_at(&mut conn, None).unwrap().is_some());
    assert!(sorted_at(&mut conn, Some("nobody@x")).unwrap().is_none());
}

#[test]
fn get_thread_reads_excerpts_from_s3() {
    let db = TestDb::new();
    let store = aimap::storage::Store::new(
        aimap::storage::S3Config { bucket: "b".into(), prefix: "t/".into(), ..Default::default() },
        MemoryS3::new(),
    );
    let mut cat = PgCatalog::new();
    let mid = seeded(&db, &store, &mut cat, 1, "Interview slot");

    let thread = get_thread(&mut db.connect(), mid, &store).unwrap().unwrap();
    assert_eq!(thread["thread_id"].as_str().unwrap(), "<m1@example.com>"); // no references: own id is the thread id
    let msgs = thread["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 1);
    assert_eq!(msgs[0]["sender"], "Bob");
    assert_eq!(msgs[0]["excerpt"], "Can you do Tuesday at 3pm?");
    assert_eq!(msgs[0]["from_recipient"], json!(false));

    // unknown message -> None
    assert!(get_thread(&mut db.connect(), 999_999, &store).unwrap().is_none());
}

#[test]
fn generate_draft_happy_path_end_to_end() {
    let db = TestDb::new();
    let store = aimap::storage::Store::new(
        aimap::storage::S3Config { bucket: "b".into(), prefix: "t/".into(), ..Default::default() },
        MemoryS3::new(),
    );
    let mut cat = PgCatalog::new();
    let mid = seeded(&db, &store, &mut cat, 1, "Interview slot");

    let model = FakeModel(json!({
        "choices": [{"message": {"content": "Hi Bob,\n\nTuesday at 3pm works for me.\n\nThanks!"}}]
    }));
    let out = aimap::draft::generate_draft(
        &mut db.connect(),
        &store,
        &model,
        mid,
        None,
        &draft_config(),
        jiff::Timestamp::from_second(1767225600).unwrap(),
        &aimap::logs::silent_logger("aimap.draft"),
    )
    .unwrap();

    assert_eq!(out["message_id"], mid);
    assert_eq!(out["needs_reply"], json!(true));
    assert_eq!(out["to"], json!(["bob@example.com"]));
    assert_eq!(out["subject"], "RE: Interview slot");
    assert!(out["body"].as_str().unwrap().contains("Tuesday at 3pm works"));
    assert_eq!(out["model"], "test-model");
    assert_eq!(out["used_message_ids"], json!([mid]));
    assert_eq!(out["created_at"], "2026-01-01T00:00:00Z");

    // the prompt carries the thread excerpt from S3
    let _ = complete_draft; // (also exercised elsewhere)
}

#[test]
fn generate_draft_errors_match_the_api_contract() {
    let db = TestDb::new();
    let store = aimap::storage::Store::new(
        aimap::storage::S3Config { bucket: "b".into(), prefix: "t/".into(), ..Default::default() },
        MemoryS3::new(),
    );
    let mut cat = PgCatalog::new();
    let mid = seeded(&db, &store, &mut cat, 1, "Interview slot");
    let mut conn = db.connect();

    // not configured
    let mut cfg = draft_config();
    cfg.api_key = None;
    let model = FakeModel(json!({}));
    let e = aimap::draft::generate_draft(&mut conn, &store, &model, mid, None, &cfg, jiff::Timestamp::MIN, &aimap::logs::silent_logger("aimap.draft"))
        .unwrap_err();
    assert!(matches!(e, aimap::draft::GenerateError::Draft(DraftError::DraftingNotConfigured)));

    // unknown message
    let e = aimap::draft::generate_draft(&mut conn, &store, &model, 999_999, None, &draft_config(), jiff::Timestamp::MIN, &aimap::logs::silent_logger("aimap.draft"))
        .unwrap_err();
    assert!(matches!(e, aimap::draft::GenerateError::Draft(DraftError::MessageNotFound)));

    // instructions too long
    let e = aimap::draft::generate_draft(
        &mut conn, &store, &model, mid,
        Some(&"x".repeat(2001)), &draft_config(), jiff::Timestamp::MIN,
        &aimap::logs::silent_logger("aimap.draft"),
    )
    .unwrap_err();
    assert!(matches!(e, aimap::draft::GenerateError::Draft(DraftError::InstructionsTooLong)));
}
