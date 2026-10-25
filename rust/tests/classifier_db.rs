//! Port of `tests/test_classifier.py`: the drain/run_forever loops, skip and
//! retry semantics, permanent failures — real Postgres, the in-memory S3
//! fake, and a fake Jev (exactly like the Python suite).

mod support;

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use aimap::catalog::PgCatalog;
use aimap::classifier::{Asker, AskError, Classifier, JevResult, StopEvent};
use aimap::config::{ClassifierSettings, S3Config};
use aimap::db::Pool;
use aimap::ingest::{sync_mailbox, MailSource, SyncError, SyncOptions};
use aimap::jev::Pattern;
use aimap::profiles::{get as profile_get, put as profile_put, replace_patterns};
use aimap::storage::Store;
use serde_json::{json, Map, Value};
use support::{MemoryS3, TestDb};

const BUCKET: &str = "aimap-test";

fn profile() -> Value {
    json!({"who": "Alex, a freelance designer.", "active_priorities": ["Client invoices"]})
}

fn patterns() -> Vec<Pattern> {
    vec![Pattern {
        insight: "Shop promotions".to_string(),
        importance: "Low".to_string(),
        action_bucket: "discard".to_string(),
        tags: vec!["promo".to_string()],
        examples: vec![json!({"from": "Shop <deals@shop.example>", "subject": "Sale"})],
    }]
}

/// A fake Jev shared between worker threads; the clone shares the call log.
struct FakeJev {
    calls: Mutex<Vec<(Value, Value)>>,
    fail_times: Mutex<i32>,
}

impl FakeJev {
    fn new(fail_times: i32) -> Arc<FakeJev> {
        Arc::new(FakeJev { calls: Mutex::new(Vec::new()), fail_times: Mutex::new(fail_times) })
    }
}

#[derive(Clone)]
struct FakeJevHandle(Arc<FakeJev>);

impl Asker for FakeJevHandle {
    fn ask(&mut self, state: &Value, questions: &Value) -> Result<JevResult, AskError> {
        let fake = &self.0;
        fake.calls.lock().unwrap().push((state.clone(), questions.clone()));
        let mut fails = fake.fail_times.lock().unwrap();
        if *fails > 0 {
            *fails -= 1;
            drop(fails);
            return Err(AskError::Api("ConnectionError: jev unreachable".to_string()));
        }
        drop(fails);
        let mut a = Map::new();
        a.insert("importance".to_string(), json!({"score": 0.2, "confidence": 0.9}));
        a.insert("action".to_string(), json!({"choice": "discard", "confidence": 0.9}));
        a.insert("sig:promotional".to_string(), json!({"noul": 0.95}));
        if questions.get("pattern").is_some() {
            a.insert(
                "pattern".to_string(),
                json!({"choice": "Shop promotions", "confidence": 0.8,
                       "probabilities": {"Shop promotions": 0.8, "none_of_these": 0.2}}),
            );
            a.insert("tag:promo".to_string(), json!({"noul": 0.9}));
        }
        Ok(JevResult {
            model: "jev-test".to_string(),
            answers: a,
            usage: Some(json!({"input_tokens": 10, "output_tokens": 2})),
        })
    }
}

/// In-memory MailSource, like conftest's FakeMailbox (uidvalidity 1).
struct FakeMailbox {
    data: std::collections::BTreeMap<i64, Vec<u8>>,
    fetched: Mutex<Vec<i64>>,
}

impl MailSource for FakeMailbox {
    fn select(&mut self, _mailbox: &str) -> Result<i64, SyncError> {
        Ok(1)
    }
    fn uids_after(&mut self, last_uid: i64) -> Result<Vec<i64>, SyncError> {
        Ok(self.data.keys().copied().filter(|u| *u > last_uid).collect())
    }
    fn fetch(&mut self, uid: i64) -> Result<Option<aimap::ingest::FetchedMessage>, SyncError> {
        let Some(raw) = self.data.get(&uid) else {
            return Ok(None);
        };
        self.fetched.lock().unwrap().push(uid);
        Ok(Some(aimap::ingest::FetchedMessage {
            uid,
            raw: raw.clone(),
            flags: vec!["\\Seen".to_string()],
            internal_date: "01-Jan-2026 10:00:00 +0000".to_string(),
        }))
    }
    fn close(&mut self) {}
}

/// conftest's email_bytes.
fn email_bytes(n: u32, body: &str) -> Vec<u8> {
    format!(
        "From: Shop <deals@shop.example>\r\nTo: me@example.com\r\nSubject: m{n}\r\n\
         Date: Thu, 01 Jan 2026 10:00:00 +0000\r\nMessage-ID: <m{n}@example.com>\r\n\r\n{body}\r\n"
    )
    .into_bytes()
}

struct Env {
    db: TestDb,
    pool: Pool,
    store: Store<MemoryS3>,
    catalog: PgCatalog,
}

impl Env {
    fn new() -> Env {
        let db = TestDb::new();
        let pool = Pool::open(&db.url(), 6).unwrap();
        let store = Store::new(
            S3Config { bucket: BUCKET.into(), prefix: "t/".into(), ..Default::default() },
            MemoryS3::new(),
        );
        Env { db, pool, store, catalog: PgCatalog::new() }
    }

    /// conftest's setup(): sync n messages, then set the profile + patterns.
    fn setup(&mut self, n: u32, with_patterns: bool) {
        let mut box_ = FakeMailbox {
            data: (1..=n as i64).map(|u| (u, email_bytes(u as u32, &format!("Sale number {u}")))).collect(),
            fetched: Mutex::new(Vec::new()),
        };
        let mut conn = self.db.connect();
        sync_mailbox(
            &mut box_,
            &self.store,
            &mut self.catalog,
            &mut conn,
            "me@example.com",
            "INBOX",
            &SyncOptions { initial_fetch_count: 0, ..Default::default() },
            &aimap::logs::silent_logger("aimap.ingest"),
        )
        .unwrap();
        profile_put(&mut conn, "default", &profile()).unwrap();
        if with_patterns {
            let pid = profile_get(&mut conn, "default").unwrap().id;
            replace_patterns(&mut conn, pid, &patterns()).unwrap();
        }
    }
}

fn make<'a>(
    env: &'a Env,
    fake: Arc<FakeJev>,
    overrides: impl FnOnce(&mut ClassifierSettings),
) -> Classifier<'a, MemoryS3, Box<dyn Fn() -> Box<dyn Asker> + Send + Sync>> {
    let mut settings = ClassifierSettings {
        model: "jev-test".to_string(),
        concurrency: 2,
        retry_delay: 0.0,
        ..Default::default()
    };
    overrides(&mut settings);
    let handle = FakeJevHandle(fake.clone());
    let factory: Box<dyn Fn() -> Box<dyn Asker> + Send + Sync> = Box::new(move || Box::new(handle.clone()));
    Classifier {
        settings,
        pool: &env.pool,
        store: &env.store,
        asker_factory: factory,
        stop: StopEvent::new(),
        idle_wait: 0.01,
        reclaim_every: 60.0,
        log: aimap::logs::silent_logger("aimap.classifier"),
    }
}

/// One row as JSON, converting each column by its Postgres type (Value
/// deserializes only from JSON/JSONB).
fn query(env: &Env, sql: &str) -> Vec<Vec<Value>> {
    let mut conn = env.db.connect();
    conn.query(sql, &[])
        .unwrap()
        .iter()
        .map(|row| (0..row.len()).map(|i| cell(row, i)).collect())
        .collect()
}

fn cell(row: &postgres::Row, i: usize) -> Value {
    use postgres::types::Type;
    let ty = row.columns()[i].type_();
    if *ty == Type::TEXT || *ty == Type::VARCHAR {
        row.get::<_, Option<String>>(i).map(Value::String).unwrap_or(Value::Null)
    } else if *ty == Type::BOOL {
        row.get::<_, Option<bool>>(i).map(Value::Bool).unwrap_or(Value::Null)
    } else if *ty == Type::INT2 {
        row.get::<_, Option<i16>>(i).map(|v| json!(v)).unwrap_or(Value::Null)
    } else if *ty == Type::INT4 {
        row.get::<_, Option<i32>>(i).map(|v| json!(v)).unwrap_or(Value::Null)
    } else if *ty == Type::INT8 {
        row.get::<_, Option<i64>>(i).map(|v| json!(v)).unwrap_or(Value::Null)
    } else if *ty == Type::FLOAT4 {
        row.get::<_, Option<f32>>(i).map(|v| json!(v)).unwrap_or(Value::Null)
    } else if *ty == Type::FLOAT8 {
        row.get::<_, Option<f64>>(i).map(|v| json!(v)).unwrap_or(Value::Null)
    } else if *ty == Type::JSON || *ty == Type::JSONB {
        row.get::<_, Option<Value>>(i).unwrap_or(Value::Null)
    } else if *ty == Type::TEXT_ARRAY || *ty == Type::VARCHAR_ARRAY {
        row.get::<_, Option<Vec<String>>>(i).map(|v| json!(v)).unwrap_or(Value::Null)
    } else {
        panic!("query helper: unsupported column type {ty}")
    }
}

fn wait_for(env: &Env, sql: &str, want: &[Vec<Value>]) -> bool {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if query(env, sql) == want {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

#[test]
fn classifies_every_queued_message() {
    let mut env = Env::new();
    env.setup(3, true);
    let fake = FakeJev::new(0);
    let c = make(&env, fake.clone(), |_| {});
    assert_eq!(c.drain(), 3);
    assert_eq!(fake.calls.lock().unwrap().len(), 3);
    let (state, questions) = &fake.calls.lock().unwrap()[0];
    assert_eq!(state["recipient_profile"], profile());
    assert!(state["email"]["body"].as_str().unwrap().starts_with("Sale number"));
    assert!(questions.get("pattern").is_some());
    let want = || -> Vec<Vec<Value>> {
        (0..3)
            .map(|_| vec![
                json!("Low"),
                json!("discard"),
                json!(["promo"]),
                json!("pattern"),
                json!(false),
                json!("jev-test"),
            ])
            .collect()
    };
    assert_eq!(
        query(
            &env,
            "SELECT importance, action_bucket, tags, source, needs_review, model FROM classifications"
        ),
        want()
    );
    assert_eq!(query(&env, "SELECT DISTINCT status FROM jobs"), vec![vec![json!("done")]]);
    assert_eq!(query(&env, "SELECT count(*) FROM message_labels"), vec![vec![json!(3)]]);
}

#[test]
fn requeued_job_with_same_inputs_is_skipped() {
    let mut env = Env::new();
    env.setup(1, true);
    let fake = FakeJev::new(0);
    let c = make(&env, fake.clone(), |_| {});
    c.drain();
    {
        let mut conn = env.db.connect();
        conn.execute("UPDATE jobs SET status = 'pending'", &[]).unwrap();
    }
    let c = make(&env, fake.clone(), |_| {});
    c.drain();
    assert_eq!(fake.calls.lock().unwrap().len(), 1);
    // a changed profile gives a new key, so the message is classified again
    {
        let mut conn = env.db.connect();
        let mut changed = profile();
        changed["who"] = json!("Alex, now a studio owner.");
        profile_put(&mut conn, "default", &changed).unwrap();
        conn.execute("UPDATE jobs SET status = 'pending'", &[]).unwrap();
    }
    let c = make(&env, fake.clone(), |_| {});
    c.drain();
    assert_eq!(fake.calls.lock().unwrap().len(), 2);
    assert_eq!(
        query(&env, "SELECT count(*) FROM classifications"),
        vec![vec![json!(2)]]
    );
}

#[test]
fn profile_per_account() {
    let mut env = Env::new();
    env.setup(1, true);
    {
        let mut conn = env.db.connect();
        profile_put(&mut conn, "work", &json!({"who": "Alex at work."})).unwrap();
        aimap::profiles::assign(&mut conn, "me@example.com", "work").unwrap();
    }
    let fake = FakeJev::new(0);
    let c = make(&env, fake.clone(), |_| {});
    c.drain();
    let (state, questions) = &fake.calls.lock().unwrap()[0];
    assert_eq!(state["recipient_profile"], json!({"who": "Alex at work."}));
    assert!(questions.get("pattern").is_none());
    assert_eq!(
        query(&env, "SELECT source FROM classifications"),
        vec![vec![json!("judgment")]]
    );
}

#[test]
fn errors_are_retried_then_succeed() {
    let mut env = Env::new();
    env.setup(1, true);
    let fake = FakeJev::new(1);
    let c = make(&env, fake.clone(), |s| s.retry_delay = 300.0);
    c.drain();
    assert_eq!(
        query(&env, "SELECT status, attempts, last_error, run_after > now() FROM jobs"),
        vec![vec![json!("pending"), json!(1), json!("ConnectionError: jev unreachable"), json!(true)]]
    );
    {
        let mut conn = env.db.connect();
        conn.execute("UPDATE jobs SET run_after = now()", &[]).unwrap();
    }
    let c = make(&env, fake.clone(), |s| s.retry_delay = 300.0);
    c.drain();
    assert_eq!(
        query(&env, "SELECT status, attempts FROM jobs"),
        vec![vec![json!("done"), json!(2)]]
    );
}

#[test]
fn missing_raw_object_fails_permanently() {
    let mut env = Env::new();
    env.setup(1, true);
    env.store.client.delete_object(
        BUCKET,
        "t/raw/me@example.com/INBOX/1/1.eml",
    );
    let fake = FakeJev::new(0);
    let c = make(&env, fake.clone(), |_| {});
    c.drain();
    let rows = query(&env, "SELECT status, last_error FROM jobs");
    assert_eq!(rows[0][0], json!("failed"));
    assert!(rows[0][1].as_str().unwrap().contains("missing from S3"), "{rows:?}");
    assert!(fake.calls.lock().unwrap().is_empty());
}

#[test]
fn run_forever_stops_on_signal() {
    let mut env = Env::new();
    env.setup(2, true);
    let fake = FakeJev::new(0);
    let c = make(&env, fake.clone(), |_| {});
    std::thread::scope(|s| {
        let worker = s.spawn(|| c.run_forever());
        assert!(wait_for(
            &env,
            "SELECT count(*) FROM classifications",
            &[vec![json!(2)]]
        ));
        c.stop.set();
        assert!(worker.join().is_ok(), "run_forever did not stop");
    });
    assert_eq!(
        query(&env, "SELECT count(*) FROM classifications"),
        vec![vec![json!(2)]]
    );
}

#[test]
fn reclaim_puts_back_dead_workers_jobs() {
    // a job left 'running' older than job_timeout goes back to pending
    let mut env = Env::new();
    env.setup(1, true);
    {
        let mut conn = env.db.connect();
        // claim the job like a worker that then died, backdate the lock
        let job = aimap::queue::claim(&mut conn, "classify").unwrap().unwrap();
        conn.execute("UPDATE jobs SET locked_at = now() - interval '1 hour'", &[])
            .unwrap();
        let c = make(&env, FakeJev::new(0), |s| s.job_timeout = 60.0);
        assert_eq!(c.reclaim().unwrap(), 1);
        let _ = job;
    }
    let fake = FakeJev::new(0);
    let c = make(&env, fake.clone(), |_| {});
    assert_eq!(c.drain(), 1);
    assert_eq!(
        query(&env, "SELECT DISTINCT status FROM jobs"),
        vec![vec![json!("done")]]
    );
}
