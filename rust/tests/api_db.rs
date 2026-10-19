//! Port of `tests/test_api.py`: the HTTP routes, driven through `api::handle`
//! (FastAPI's TestClient role) against a real Postgres and the in-memory S3
//! fake, with a fake verifier — plus one full-server test over a real socket.

mod support;

use aimap::api::{handle, AppState, ApiResponse, Verifier};
use aimap::auth::{User, VerifyError};
use aimap::catalog::PgCatalog;
use aimap::config::S3Config;
use aimap::db::Pool;
use aimap::draft::{ChatTransport, DraftConfig, ModelCall};
use aimap::ingest::{sync_mailbox, FetchedMessage, MailSource, SyncError, SyncOptions};
use aimap::logs::Logger;
use aimap::storage::Store;
use postgres::Client;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use support::{MemoryS3, TestDb};

const ME: &str = "me@example.com";
const BUCKET: &str = "aimap-test";

fn auth_headers() -> Vec<(String, String)> {
    vec![("Authorization".to_string(), "Bearer good".to_string())]
}

/// The `FakeVerifier`: "good" is the allowed user, "stranger" is forbidden,
/// everything else invalid.
struct FakeVerifier;

impl Verifier for FakeVerifier {
    fn verify(&self, token: &str) -> Result<User, VerifyError> {
        match token {
            "good" => Ok(User { uid: "u1".into(), email: ME.into() }),
            "stranger" => Err(VerifyError::Forbidden(aimap::auth::Forbidden("email not allowed".into()))),
            _ => Err(VerifyError::Auth(aimap::auth::AuthError("invalid token".into()))),
        }
    }
}

/// The transport the draft tests fake, like the Python tests patch
/// `requests.post`: always this reply body.
struct FakeChat(Value);

impl ChatTransport for FakeChat {
    fn post(&self, _call: &ModelCall) -> Result<Value, aimap::draft::TransportError> {
        Ok(self.0.clone())
    }
}

fn empty_transport() -> Arc<dyn ChatTransport> {
    Arc::new(FakeChat(json!({"choices": [{"message": {"content": "A reply."}}]})))
}

// --------------------------------------------------------------------------------------
// the conftest pieces: email_bytes + FakeMailbox
// --------------------------------------------------------------------------------------

fn email_bytes(n: u32, message_id: Option<&str>, sender: &str, subject: &str, body: &str, extra: &str) -> Vec<u8> {
    let default_mid = format!("<m{n}@example.com>");
    let mid = message_id.unwrap_or(default_mid.as_str());
    let mut headers = format!(
        "From: {sender}\r\nTo: {ME}\r\nSubject: {subject}\r\n\
         Date: Thu, 01 Jan 2026 10:00:00 +0000\r\n"
    );
    if !mid.is_empty() {
        headers.push_str(&format!("Message-ID: {mid}\r\n"));
    }
    format!("{headers}{extra}\r\n{body}\r\n").into_bytes()
}

fn replace_first(bytes: &[u8], from: &[u8], to: &[u8]) -> Vec<u8> {
    match bytes.windows(from.len()).position(|w| w == from) {
        Some(i) => {
            let mut out = bytes[..i].to_vec();
            out.extend_from_slice(to);
            out.extend_from_slice(&bytes[i + from.len()..]);
            out
        }
        None => bytes.to_vec(),
    }
}

type MailboxSpec<'a> = Vec<(&'a str, i64, Vec<(i64, Vec<u8>)>)>;

type MailboxData = BTreeMap<String, (i64, BTreeMap<i64, Vec<u8>>)>;

struct FakeMailbox {
    mailboxes: Mutex<MailboxData>,
    current: Option<String>,
}

impl FakeMailbox {
    fn new(mailboxes: MailboxSpec<'_>) -> FakeMailbox {
        FakeMailbox {
            mailboxes: Mutex::new(
                mailboxes
                    .into_iter()
                    .map(|(name, uv, msgs)| (name.to_string(), (uv, msgs.into_iter().collect())))
                    .collect(),
            ),
            current: None,
        }
    }
}

impl MailSource for FakeMailbox {
    fn select(&mut self, mailbox: &str) -> Result<i64, SyncError> {
        self.mailboxes
            .lock()
            .unwrap()
            .get(mailbox)
            .map(|(uv, _)| {
                self.current = Some(mailbox.to_string());
                *uv
            })
            .ok_or_else(|| SyncError::Imap(format!("no mailbox {mailbox}")))
    }

    fn uids_after(&mut self, last_uid: i64) -> Result<Vec<i64>, SyncError> {
        let boxes = self.mailboxes.lock().unwrap();
        let (_, data) = &boxes[self.current.as_ref().unwrap()];
        Ok(data.keys().copied().filter(|u| *u > last_uid).collect())
    }

    fn fetch(&mut self, uid: i64) -> Result<Option<FetchedMessage>, SyncError> {
        let boxes = self.mailboxes.lock().unwrap();
        let (_, data) = &boxes[self.current.as_ref().unwrap()];
        Ok(data.get(&uid).map(|raw| FetchedMessage {
            uid,
            raw: raw.clone(),
            flags: vec!["\\Seen".to_string()], // FakeMailbox marks everything \Seen
            internal_date: "01-Jan-2026 10:00:00 +0000".to_string(),
        }))
    }

    fn close(&mut self) {}
}

// --------------------------------------------------------------------------------------
// the fixtures
// --------------------------------------------------------------------------------------

struct Env {
    state: Arc<AppState<MemoryS3>>,
    db: TestDb,
    s3: MemoryS3,
    log: Logger,
}

impl Env {
    fn new() -> Env {
        let db = TestDb::new();
        let s3 = MemoryS3::new();
        let store = Store::new(S3Config { bucket: BUCKET.into(), ..Default::default() }, s3.clone());
        let pool = Pool::open(&db.url(), 4).expect("pool");
        let log = Logger::new(aimap::logs::Capture::new(), "INFO", "aimap.api.test");
        let state = Arc::new(AppState {
            pool,
            store,
            verifier: Arc::new(FakeVerifier),
            draft_config: Some(DraftConfig {
                base_url: "https://model.example".into(),
                api_key: Some("k".into()),
                model: "test-model".into(),
                timeout_secs: 5,
                max_tokens: 700,
                reasoning_effort: "low".into(),
            }),
            draft_transport: empty_transport(),
            log: log.clone(),
        });
        Env { state, db, s3, log }
    }

    fn connect(&self) -> Client {
        self.db.connect()
    }

    fn get(&self, path: &str) -> ApiResponse {
        handle(&self.state, "GET", path, "", &auth_headers(), b"")
    }

    fn get_query(&self, path: &str, query: &str) -> ApiResponse {
        handle(&self.state, "GET", path, query, &auth_headers(), b"")
    }

    fn get_as(&self, path: &str, headers: &[(String, String)]) -> ApiResponse {
        handle(&self.state, "GET", path, "", headers, b"")
    }

    fn act(&self, message_id: i64, action: &str) -> ApiResponse {
        handle(
            &self.state,
            "POST",
            &format!("/messages/{message_id}/actions"),
            "",
            &auth_headers(),
            json!({"action": action}).to_string().as_bytes(),
        )
    }

    /// The seeded fixture: four messages from the last day — act_now, reply,
    /// skim (read), and one not classified yet. Returns subject → id.
    fn seeded() -> (Env, BTreeMap<String, i64>) {
        let env = Env::new();
        let now_ts = jiff::Timestamp::now();
        /// (uid, sender, subject, bucket, importance, priority, signals)
        type Spec = (i64, &'static str, &'static str, &'static str, Option<&'static str>, f64, Value);
        let specs: Vec<Spec> = vec![
            (1, "Priya <priya@legal.example>", "Sign-off needed", "act_now", Some("Medium"), 9.0, json!({"time_sensitive": 0.9, "real_person": 0.8})),
            (2, "Elena <elena@example.com>", "Offer letter", "reply", Some("Medium"), 4.0, json!({"real_person": 0.95})),
            (3, "News <news@list.example>", "Weekly digest", "skim", Some("Medium"), -1.0, json!({"promotional": 0.2})),
            (4, "Sam <sam@example.com>", "PR notes", "", None, 0.0, json!({})),
        ];
        let mut raws: BTreeMap<i64, Vec<u8>> = BTreeMap::new();
        for (n, sender, subject, _, _, _, _) in &specs {
            let sent_at = now_ts - Duration::from_secs(*n as u64 * 3600);
            let date_header = format!("Date: {}\r\n", rfc2822(sent_at));
            let raw = email_bytes(*n as u32, None, sender, subject, &format!("Body of message {n}."), &date_header);
            // the conftest replaces the default Date header away; the extra one rules
            let raw = replace_first(&raw, b"Date: Thu, 01 Jan 2026 10:00:00 +0000\r\n", b"");
            raws.insert(*n, raw);
        }
        let mut mailbox = FakeMailbox::new(vec![("INBOX", 1, raws.into_iter().collect())]);
        let mut catalog = PgCatalog::new();
        let mut conn = env.connect();
        sync_mailbox(
            &mut mailbox,
            &env.state.store,
            &mut catalog,
            &mut conn,
            ME,
            "INBOX",
            &SyncOptions { initial_fetch_count: 0, ..Default::default() },
        )
        .expect("seed sync");

        let ids: BTreeMap<String, i64> = conn
            .query("SELECT subject, id FROM messages", &[])
            .unwrap()
            .iter()
            .map(|r| (r.get::<_, String>(0), r.get::<_, i64>(1)))
            .collect();
        let pid: i64 = conn
            .query_one("SELECT id FROM profiles WHERE name = 'default'", &[])
            .unwrap()
            .get(0);
        // FakeMailbox marks everything \Seen. Leave only the digest read.
        conn.execute(
            "UPDATE message_locations SET flags = '{}' WHERE message_id <> $1",
            &[&ids["Weekly digest"]],
        )
        .unwrap();
        for (_, _, subject, bucket, importance, priority, signals) in &specs {
            if bucket.is_empty() {
                continue;
            }
            conn.execute(
                "INSERT INTO classifications (message_id, profile_id, classifier_key, model, raw_answers,
                    importance, action_bucket, tags, insight, source, needs_review, priority, signals)
                 VALUES ($1, $2, 'k', 'jev-test', '{}', $3, $4, $5, NULL, 'judgment', false, $6, $7)",
                &[
                    &ids[*subject],
                    &pid,
                    importance,
                    bucket,
                    &if *bucket == "skim" { vec!["newsletter".to_string()] } else { Vec::new() },
                    &(*priority as f32),
                    signals,
                ],
            )
            .unwrap();
        }
        (env, ids)
    }
}

/// "Tue, 21 Oct 2025 10:00:00 +0000" — the header format the Python fixture
/// writes (`strftime("%a, %d %b %Y %H:%M:%S +0000")`).
fn rfc2822(ts: jiff::Timestamp) -> String {
    let dt = ts.to_zoned(jiff::tz::TimeZone::UTC).datetime();
    let month = match dt.month() {
        1 => "Jan", 2 => "Feb", 3 => "Mar", 4 => "Apr", 5 => "May", 6 => "Jun",
        7 => "Jul", 8 => "Aug", 9 => "Sep", 10 => "Oct", 11 => "Nov", _ => "Dec",
    };
    let day = match dt.weekday() {
        jiff::civil::Weekday::Monday => "Mon", jiff::civil::Weekday::Tuesday => "Tue",
        jiff::civil::Weekday::Wednesday => "Wed", jiff::civil::Weekday::Thursday => "Thu",
        jiff::civil::Weekday::Friday => "Fri", jiff::civil::Weekday::Saturday => "Sat",
        jiff::civil::Weekday::Sunday => "Sun",
    };
    format!("{day}, {:02} {month} {} {:02}:{:02}:{:02} +0000", dt.day(), dt.year(), dt.hour(), dt.minute(), dt.second())
}

// --------------------------------------------------------------------------------------
// the suite
// --------------------------------------------------------------------------------------

#[test]
fn healthz_needs_no_token() {
    let env = Env::new();
    let body = env.get("/healthz").body;
    assert_eq!(body, json!({"ok": true}));
}

#[test]
fn every_data_route_needs_an_allowed_user() {
    let (env, _) = Env::seeded();
    for path in ["/me", "/home", "/messages", "/messages/1", "/messages/1/body", "/accounts"] {
        for (headers, status) in [
            (vec![], 401),
            (vec![("Authorization".into(), "Basic x".into())], 401),
            (vec![("Authorization".into(), "Bearer bad".into())], 401),
            (vec![("Authorization".into(), "Bearer stranger".into())], 403),
        ] {
            let resp = env.get_as(path, &headers);
            assert_eq!(resp.status, status, "{path} with {headers:?}");
        }
    }
}

#[test]
fn actions_need_an_allowed_user() {
    let (env, _) = Env::seeded();
    for (headers, status) in [
        (vec![], 401),
        (vec![("Authorization".into(), "Bearer bad".into())], 401),
        (vec![("Authorization".into(), "Bearer stranger".into())], 403),
    ] {
        let resp = handle(
            &env.state,
            "POST",
            "/messages/1/actions",
            "",
            &headers,
            json!({"action": "handled"}).to_string().as_bytes(),
        );
        assert_eq!(resp.status, status);
    }
}

#[test]
fn me() {
    let env = Env::new();
    assert_eq!(env.get("/me").body, json!({"uid": "u1", "email": ME}));
}

#[test]
fn home_sections() {
    let (env, _) = Env::seeded();
    let body = env.get("/home").body;
    let act_now = body["act_now"].as_array().unwrap();
    assert_eq!(act_now.len(), 1, "{body}");
    assert_eq!(act_now[0]["subject"], "Sign-off needed");
    assert_eq!(
        act_now[0]["reasons"],
        json!(["Mentions a deadline", "Written to you by a person"])
    );
    let waiting = body["waiting"].as_array().unwrap();
    assert_eq!(waiting.len(), 1, "{body}");
    assert_eq!(waiting[0]["subject"], "Offer letter");
    let sorted = body["sorted"].as_array().unwrap();
    assert_eq!(sorted.len(), 1);
    assert_eq!(sorted[0]["name"], "newsletter");
    assert_eq!(sorted[0]["count"], 1);
    assert_eq!(sorted[0]["unread"], 0);
    assert_eq!(sorted[0]["summary"], "News");
    assert!(sorted[0]["latest_at"].is_string(), "{}", sorted[0]);
    assert_eq!(body["brief"]["new"], 4);
    assert_eq!(body["brief"]["unclassified"], 1);
    assert_eq!(act_now[0]["profile"], "default");
    assert!(!body["brief"]["sorted_at"].is_null());
    assert_eq!(body["brief"]["by_hour"].as_array().unwrap().len(), 24);
}

#[test]
fn action_round_trip_is_idempotent() {
    let (env, ids) = Env::seeded();
    let mid = ids["Sign-off needed"];
    let first = env.act(mid, "handled");
    assert_eq!(first.status, 200);
    assert_eq!(first.body["message_id"], mid);
    assert_eq!(first.body["state"], "handled");
    assert!(first.body["changed_at"].is_string());
    assert_eq!(env.act(mid, "handled").body["state"], "handled");
    assert_eq!(env.act(mid, "later").body["state"], "later");
    assert_eq!(env.get(&format!("/messages/{mid}")).body["state"], "later");

    let undo = env.act(mid, "undo");
    assert_eq!(undo.body, json!({"message_id": mid, "state": null, "changed_at": null}));
    assert_eq!(env.act(mid, "undo").body["state"], Value::Null); // undo twice is still fine
    assert_eq!(env.get(&format!("/messages/{mid}")).body["state"], Value::Null);
    let n: i64 = env.connect().query_one("SELECT count(*) FROM message_state", &[]).unwrap().get(0);
    assert_eq!(n, 0);
}

#[test]
fn action_records_the_signed_in_email() {
    let (env, ids) = Env::seeded();
    env.act(ids["Offer letter"], "later");
    let rows = env.connect().query("SELECT changed_by FROM message_state", &[]).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get::<_, String>(0), ME);
}

#[test]
fn action_rejects_unknown_ids_and_bad_actions() {
    let (env, ids) = Env::seeded();
    assert_eq!(env.act(999999, "handled").status, 404);
    assert_eq!(env.act(ids["PR notes"], "archive").status, 422);
    let resp = handle(
        &env.state,
        "POST",
        &format!("/messages/{}/actions", ids["PR notes"]),
        "",
        &auth_headers(),
        b"{}",
    );
    assert_eq!(resp.status, 422);
}

#[test]
fn home_leaves_handled_messages_out() {
    let (env, ids) = Env::seeded();
    env.act(ids["Sign-off needed"], "handled");
    env.act(ids["Offer letter"], "handled");
    let body = env.get("/home").body;
    assert_eq!(body["act_now"], json!([]));
    assert_eq!(body["waiting"], json!([]));
    assert_eq!(body["brief"]["act_now"], 0);
    assert_eq!(body["brief"]["waiting"], 0);
    assert_eq!(body["brief"]["handled_today"], 2);
    assert_eq!(body["brief"]["new"], 4); // they still arrived
    let other = env.get_query("/home", "account=other@example.com").body;
    assert_eq!(other["brief"]["handled_today"], 0); // handled_today follows the account filter
}

#[test]
fn home_counts_only_todays_handled() {
    let (env, ids) = Env::seeded();
    env.act(ids["Sign-off needed"], "handled");
    assert_eq!(env.get("/home").body["brief"]["handled_today"], 1);
    {
        let mut conn = env.connect();
        conn.execute("UPDATE message_state SET changed_at = now() - interval '2 days'", &[]).unwrap();
    }
    assert_eq!(env.get("/home").body["brief"]["handled_today"], 0);
    env.act(ids["Offer letter"], "later"); // later is not handled
    assert_eq!(env.get("/home").body["brief"]["handled_today"], 0);
}

#[test]
fn home_sorts_later_cards_last() {
    let (env, ids) = Env::seeded();
    {
        let mut conn = env.connect(); // a second act_now card, lower priority than the seeded one
        let pid: i64 = conn.query_one("SELECT id FROM profiles WHERE name = 'default'", &[]).unwrap().get(0);
        conn.execute(
            "INSERT INTO classifications (message_id, profile_id, classifier_key, model, raw_answers,
                importance, action_bucket, tags, source, needs_review, priority, signals)
             VALUES ($1, $2, 'k2', 'jev-test', '{}', 'Medium', 'act_now', '{}', 'judgment', false, 1.0, '{}')",
            &[&ids["PR notes"], &pid],
        )
        .unwrap();
    }
    let body = env.get("/home").body;
    let subjects: Vec<&str> = body["act_now"].as_array().unwrap().iter().map(|c| c["subject"].as_str().unwrap()).collect();
    assert_eq!(subjects, vec!["Sign-off needed", "PR notes"]);

    env.act(ids["Sign-off needed"], "later");
    let body = env.get("/home").body;
    let cards = body["act_now"].as_array().unwrap();
    let subjects: Vec<&str> = cards.iter().map(|c| c["subject"].as_str().unwrap()).collect();
    assert_eq!(subjects, vec!["PR notes", "Sign-off needed"]);
    let states: Vec<Value> = cards.iter().map(|c| c["state"].clone()).collect();
    assert_eq!(states, vec![Value::Null, json!("later")]);

    env.act(ids["PR notes"], "later"); // marked last, so it now sorts last
    let body = env.get("/home").body;
    let subjects: Vec<&str> = body["act_now"].as_array().unwrap().iter().map(|c| c["subject"].as_str().unwrap()).collect();
    assert_eq!(subjects, vec!["Sign-off needed", "PR notes"]);
}

#[test]
fn home_traffic_uses_the_time_zone() {
    let (env, _) = Env::seeded();
    let body = env.get_query("/home", "tz=Pacific/Kiritimati").body;
    let hours: Vec<i64> = body["brief"]["by_hour"].as_array().unwrap().iter().map(|h| h["hour"].as_i64().unwrap()).collect();
    assert_eq!(hours, (0..24).collect::<Vec<_>>());
    assert_eq!(env.get_query("/home", "tz=Mars/Olympus").status, 400);
}

#[test]
fn home_filters_by_account() {
    let (env, _) = Env::seeded();
    let body = env.get_query("/home", "account=other@example.com").body;
    assert_eq!(body["act_now"], json!([]));
    assert_eq!(body["brief"]["new"], 0);
}

#[test]
fn messages_pages_newest_first() {
    let (env, _) = Env::seeded();
    let first = env.get_query("/messages", "limit=3").body;
    let subjects: Vec<&str> = first["messages"].as_array().unwrap().iter().map(|m| m["subject"].as_str().unwrap()).collect();
    assert_eq!(subjects, vec!["Sign-off needed", "Offer letter", "Weekly digest"]);
    let cursor = first["next_cursor"].as_str().unwrap().to_string();
    let rest = env.get_query("/messages", &format!("limit=3&cursor={cursor}")).body;
    let subjects: Vec<&str> = rest["messages"].as_array().unwrap().iter().map(|m| m["subject"].as_str().unwrap()).collect();
    assert_eq!(subjects, vec!["PR notes"]);
    assert_eq!(rest["messages"][0]["action_bucket"], Value::Null);
    assert_eq!(rest["next_cursor"], Value::Null);
}

#[test]
fn messages_filters() {
    let (env, _) = Env::seeded();
    let unread = env.get_query("/messages", "filter=unread").body["messages"].as_array().unwrap().clone();
    assert!(!unread.iter().any(|m| m["subject"] == "Weekly digest"));
    assert_eq!(unread.len(), 3);
    assert_eq!(env.get_query("/messages", "filter=bogus").status, 422);
    assert_eq!(env.get_query("/messages", "cursor=%%%").status, 400);
}

#[test]
fn message_detail_and_body() {
    let (env, ids) = Env::seeded();
    let mid = ids["Sign-off needed"];
    let m = env.get(&format!("/messages/{mid}")).body;
    assert_eq!(m["sender"], "Priya");
    assert_eq!(m["labels"]["action_bucket"], "act_now");
    assert_eq!(m["account"], ME); // the same profile name Home cards carry
    assert_eq!(m["profile"], "default");
    assert_eq!(m["labels"]["reasons"], json!(["Mentions a deadline", "Written to you by a person"]));
    assert_eq!(m["mailboxes"], json!([{"mailbox": "INBOX", "flags": []}]));
    assert!(m.get("s3_key").is_none());
    let body = env.get(&format!("/messages/{mid}/body")).body;
    assert_eq!(body["text"], "Body of message 1.");
    assert_eq!(env.get("/messages/999999").status, 404);
}

#[test]
fn body_missing_from_bucket_is_404() {
    let (env, ids) = Env::seeded();
    let mid = ids["PR notes"];
    for key in env.state.store.list_raw().unwrap() {
        env.s3.delete_object(BUCKET, &key);
    }
    assert_eq!(env.get(&format!("/messages/{mid}/body")).status, 404);
}

#[test]
fn accounts() {
    let (env, _) = Env::seeded();
    let body = env.get("/accounts").body;
    assert_eq!(
        body,
        json!({"accounts": [{"address": ME, "profile": "default", "messages": 4, "unread": 3}]})
    );
}

#[test]
fn reply_detection() {
    let (env, ids) = Env::seeded();
    let m = env.get(&format!("/messages/{}", ids["Sign-off needed"])).body;
    assert_eq!(m["reply"], json!({"needed": true, "reason": "Mentions a deadline"}));
    let m = env.get(&format!("/messages/{}", ids["Offer letter"])).body;
    assert_eq!(m["reply"]["needed"], true);
    let m = env.get(&format!("/messages/{}", ids["Weekly digest"])).body;
    assert_eq!(m["reply"], json!({"needed": false, "reason": null}));
    let m = env.get(&format!("/messages/{}", ids["PR notes"])).body;
    assert!(m["reply"].is_null());
}

#[test]
fn thread_endpoint() {
    let (env, ids) = Env::seeded();
    let mid = ids["Sign-off needed"];
    let resp = env.get(&format!("/messages/{mid}/thread"));
    assert_eq!(resp.status, 200);
    let data = resp.body;
    assert_eq!(data["message_id"], mid);
    assert!(data.get("thread_id").is_some());
    let messages = data["messages"].as_array().unwrap();
    assert!(!messages.is_empty());
    let msg = &messages[0];
    assert_eq!(msg["message_id"], mid);
    assert_eq!(msg["sender"], "Priya");
    assert_eq!(msg["from_recipient"], false);
    assert!(msg.get("excerpt").is_some());
    assert_eq!(env.get("/messages/999999/thread").status, 404);
}

#[test]
fn thread_groups_messages_by_references_and_strips_quoted_text() {
    let env = Env::new();
    let original_body = "Jorge, can you please schedule the Solera interview for next week? Thanks, Javier";
    let reply_body = "Jorge, by any chance can you please re schedule for Tuesday or Wednesday after 3pm?\r\n\r\n\
-----Original Appointment-----\r\n\
From: Jorge Vilca <jorge.vilca@example.com>\r\n\
Sent: Monday, June 1, 2025 5:30 PM\r\n\
To: Barron, Javier <javier.barron@solera.com>\r\n\
Subject: Jorge - Solera Interview - SW Mgr\r\n\r\n\
Jorge, can you please schedule the Solera interview for next week? Thanks, Javier";
    let now = jiff::Timestamp::now() - Duration::from_secs(24 * 3600);
    let original = email_bytes(1, Some("<original@example.com>"), "Javier Barron <javier.barron@solera.com>",
        "Jorge - Solera Interview - SW Mgr", original_body, &format!("Date: {}\r\n", rfc2822(now)));
    let reply = email_bytes(2, Some("<reply@example.com>"), "Javier Barron <javier.barron@solera.com>",
        "RE: Jorge - Solera Interview - SW Mgr", reply_body,
        &format!(
            "Date: {}\r\nIn-Reply-To: <original@example.com>\r\nReferences: <original@example.com>\r\n",
            rfc2822(jiff::Timestamp::now())
        ));
    let mut mailbox = FakeMailbox::new(vec![("INBOX", 1, vec![(1, original), (2, reply)])]);
    let mut catalog = PgCatalog::new();
    {
        let mut conn = env.connect();
        sync_mailbox(&mut mailbox, &env.state.store, &mut catalog, &mut conn, ME, "INBOX",
            &SyncOptions { initial_fetch_count: 0, ..Default::default() })
            .expect("sync");
    }
    let reply_id: i64 = env
        .connect()
        .query_one("SELECT id FROM messages WHERE rfc822_message_id = '<reply@example.com>'", &[])
        .unwrap()
        .get(0);

    let resp = env.get(&format!("/messages/{reply_id}/thread"));
    assert_eq!(resp.status, 200);
    let data = resp.body;
    assert_eq!(data["message_id"], reply_id);
    let messages = data["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 2);

    let newest = &messages[0];
    let oldest = &messages[1];
    assert_eq!(newest["subject"], "RE: Jorge - Solera Interview - SW Mgr");
    assert_eq!(oldest["subject"], "Jorge - Solera Interview - SW Mgr");
    assert_eq!(newest["sender"], "Javier Barron");
    assert_eq!(oldest["sender"], "Javier Barron");

    let newest_excerpt = newest["excerpt"].as_str().unwrap();
    let oldest_excerpt = oldest["excerpt"].as_str().unwrap();
    assert!(!newest_excerpt.contains("-----Original Appointment-----"));
    assert!(newest_excerpt.contains("re schedule"));
    assert!(oldest_excerpt.contains("schedule the Solera interview"));
}

// --------------------------------------------------------------------------------------
// /draft through the route
// --------------------------------------------------------------------------------------

#[test]
fn draft_route_end_to_end() {
    let (env, ids) = Env::seeded();
    let mid = ids["Offer letter"];
    let resp = handle(
        &env.state,
        "POST",
        &format!("/messages/{mid}/draft"),
        "",
        &auth_headers(),
        json!({"instructions": "mention the start date"}).to_string().as_bytes(),
    );
    assert_eq!(resp.status, 200, "{:?}", resp.body);
    assert_eq!(resp.body["subject"], "RE: Offer letter");
    assert_eq!(resp.body["body"], "A reply.");
    assert_eq!(resp.body["to"], json!(["elena@example.com"]));
    assert_eq!(resp.body["needs_reply"], true);
    assert_eq!(resp.body["used_message_ids"].as_array().unwrap().len(), 1);
}

#[test]
fn draft_route_validates_the_body_and_the_config() {
    let (env, ids) = Env::seeded();
    let mid = ids["Offer letter"];
    let long = "x".repeat(2001);
    let resp = handle(&env.state, "POST", &format!("/messages/{mid}/draft"), "", &auth_headers(),
        json!({"instructions": long}).to_string().as_bytes());
    assert_eq!(resp.status, 422);

    let resp = handle(&env.state, "POST", &format!("/messages/{mid}/draft"), "", &auth_headers(), b"{}");
    assert_eq!(resp.status, 200);

    let resp = handle(&env.state, "POST", "/messages/999999/draft", "", &auth_headers(),
        json!({}).to_string().as_bytes());
    assert_eq!(resp.status, 404);

    // without a configured key the endpoint refuses with 503
    let unconfigured = AppState {
        draft_config: None,
        draft_transport: empty_transport(),
        verifier: Arc::new(FakeVerifier),
        pool: env.state.pool.clone(),
        store: env.state.store.clone(),
        log: env.log.clone(),
    };
    let resp = handle(&unconfigured, "POST", &format!("/messages/{mid}/draft"), "", &auth_headers(),
        json!({}).to_string().as_bytes());
    assert_eq!(resp.status, 503);
    assert_eq!(resp.body["detail"], "Drafting is not configured.");
}

// --------------------------------------------------------------------------------------
// the real server over a socket (one smoke pass; the wire-level behavior is
// pinned by the httpc unit tests)
// --------------------------------------------------------------------------------------

#[test]
fn serve_smoke_over_tcp() {
    let env = Env::new();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().unwrap().port();
    let state = env.state.clone();
    let server = std::thread::spawn(move || aimap::api::serve_listener(listener, state));

    let req = aimap::httpc::Request {
        method: "GET",
        url: &format!("http://127.0.0.1:{port}/healthz"),
        headers: &[],
        body: None,
        timeout: Duration::from_secs(10),
    };
    let resp = aimap::httpc::request(&req).expect("healthz over real TCP");
    assert_eq!(resp.status, 200);
    assert_eq!(serde_json::from_slice::<Value>(&resp.body).unwrap(), json!({"ok": true}));

    let req = aimap::httpc::Request {
        method: "GET",
        url: &format!("http://127.0.0.1:{port}/me"),
        headers: &[],
        body: None,
        timeout: Duration::from_secs(10),
    };
    let resp = aimap::httpc::request(&req).expect("401 over real TCP");
    assert_eq!(resp.status, 401);
    assert_eq!(resp.header("WWW-Authenticate"), Some("Bearer"));

    let _ = server.is_finished(); // the server thread runs for the process lifetime
}
