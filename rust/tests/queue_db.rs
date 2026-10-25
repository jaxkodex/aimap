//! Port of `tests/test_queue.py` and `tests/test_db.py` against a real
//! Postgres (`TEST_DATABASE_URL`, created + migrated per test).

mod support;

use aimap::db;
use aimap::queue;
use support::TestDb;

/// Seed n jobs the way the tests do: record a batch through the catalog path
/// (record_batch queues a classify job per message). We insert directly with
/// the same SQL until the catalog port arrives.
fn seed(db: &TestDb, n: i64) {
    let mut conn = db.connect();
    // one account + n messages, each with a queued classify job
    conn.execute(
        "INSERT INTO accounts (address, profile_id) SELECT 'me@example.com', id FROM profiles LIMIT 1",
        &[],
    ).unwrap();
    let account_id: i64 = conn.query_one("SELECT id FROM accounts LIMIT 1", &[]).unwrap().get(0);
    for uid in 1..=n {
        let mid: i64 = conn.query_one(
            "INSERT INTO messages (account_id, rfc822_message_id, thread_id)
             VALUES ($1, $2, 'thread:test') RETURNING id",
            &[&account_id, &format!("<seed-{uid}@x>")],
        ).unwrap().get(0);
        conn.execute(
            "INSERT INTO jobs (message_id, stage) VALUES ($1, 'classify')",
            &[&mid],
        ).unwrap();
    }
}

fn status(db: &TestDb) -> Vec<(String, i32, Option<String>)> {
    let mut conn = db.connect();
    conn.query("SELECT status, attempts, last_error FROM jobs ORDER BY id", &[])
        .unwrap()
        .iter()
        .map(|r| (r.get(0), r.get(1), r.get(2)))
        .collect()
}

#[test]
fn two_claimers_never_get_the_same_job() {
    let db = TestDb::new();
    seed(&db, 20);
    let got = std::sync::Mutex::new(Vec::new());
    let barrier = std::sync::Barrier::new(4);

    std::thread::scope(|scope| {
        for _ in 0..4 {
            scope.spawn(|| {
                barrier.wait();
                loop {
                    let mut conn = db.connect();
                    let job = queue::claim(&mut conn, "classify").unwrap();
                    match job {
                        None => return,
                        Some(j) => got.lock().unwrap().push(j.id),
                    }
                }
            });
        }
    });

    let mut ids = got.into_inner().unwrap();
    ids.sort();
    assert_eq!(ids, (1..=20).collect::<Vec<i64>>());
}

#[test]
fn claim_holds_row_lock_until_commit() {
    let db = TestDb::new();
    seed(&db, 1);
    let mut a = db.connect();
    let mut b = db.connect();

    let mut tx_a = a.transaction().unwrap();
    assert!(queue::claim(&mut tx_a, "classify").unwrap().is_some()); // a's transaction still open
    assert!(queue::claim(&mut b, "classify").unwrap().is_none()); // skipped, not blocked
    tx_a.commit().unwrap();
    // after commit the row is 'running', so a fresh claim finds nothing
    assert!(queue::claim(&mut b, "classify").unwrap().is_none());
}

#[test]
fn retry_backoff_then_failed() {
    let db = TestDb::new();
    seed(&db, 1);
    for attempt in 1..=2 {
        let mut conn = db.connect();
        conn.execute("UPDATE jobs SET run_after = now()", &[]).unwrap();
        let job = queue::claim(&mut conn, "classify").unwrap().unwrap();
        assert_eq!(job.attempts, attempt);
        let status = queue::fail(&mut conn, &job, "boom", 2, 60.0, 3600.0, false).unwrap();
        assert_eq!(status, if attempt == 1 { "pending" } else { "failed" });
        drop(conn);
        if attempt == 1 {
            let mut conn = db.connect();
            assert!(queue::claim(&mut conn, "classify").unwrap().is_none()); // waiting out the backoff
        }
    }
    assert_eq!(status(&db), vec![("failed".to_string(), 2, Some("boom".to_string()))]);
    {
        let mut conn = db.connect();
        assert_eq!(queue::requeue(&mut conn, "classify", &["failed"]).unwrap(), 1);
    }
    assert_eq!(status(&db), vec![("pending".to_string(), 0, None)]);
}

#[test]
fn permanent_failure_skips_retries() {
    let db = TestDb::new();
    seed(&db, 1);
    let mut conn = db.connect();
    let job = queue::claim(&mut conn, "classify").unwrap().unwrap();
    let status = queue::fail(&mut conn, &job, "gone", 5, 1.0, 3600.0, true).unwrap();
    assert_eq!(status, "failed");
}

#[test]
fn stuck_jobs_are_reclaimed() {
    let db = TestDb::new();
    seed(&db, 2);
    {
        let mut conn = db.connect();
        queue::claim(&mut conn, "classify").unwrap();
        queue::claim(&mut conn, "classify").unwrap();
        conn.execute("UPDATE jobs SET locked_at = now() - interval '1 hour' WHERE id = 1", &[]).unwrap();
    }
    {
        let mut conn = db.connect();
        assert_eq!(queue::reclaim_stuck(&mut conn, "classify", 600.0, 5).unwrap(), 1);
    }
    let statuses: Vec<String> = status(&db).into_iter().map(|(s, _, _)| s).collect();
    assert_eq!(statuses, vec!["pending", "running"]);
    let mut conn = db.connect();
    let counts = queue::counts(&mut conn).unwrap();
    let by_status: std::collections::HashMap<String, i64> =
        counts.into_iter().map(|(_, s, n)| (s, n)).collect();
    assert_eq!(by_status.get("pending"), Some(&1));
    assert_eq!(by_status.get("running"), Some(&1));
}

// ---- db.py ports ---------------------------------------------------------

#[test]
fn migrations_are_ordered_and_numbered() {
    let ms = db::migrations().unwrap();
    assert!(!ms.is_empty());
    let versions: Vec<i32> = ms.iter().map(|m| m.version).collect();
    let mut sorted = versions.clone();
    sorted.sort();
    assert_eq!(versions, sorted);
    assert_eq!(ms[0].name, "0001_init.sql");
}

#[test]
fn migrate_is_idempotent() {
    let db = TestDb::new(); // TestDb::new already migrated
    let mut conn = db.connect();
    assert_eq!(db::migrate_on(&mut conn, &aimap::logs::silent_logger("aimap.db")).unwrap(), Vec::<String>::new());
    assert!(db::pending(&mut conn).unwrap().is_empty());
    let applied = db::applied_versions(&mut conn).unwrap();
    let all: std::collections::HashSet<i32> = db::migrations().unwrap().into_iter().map(|m| m.version).collect();
    assert_eq!(applied, all);
}

#[test]
fn require_current_passes_on_current_schema() {
    let db = TestDb::new();
    let mut conn = db.connect();
    db::require_current(&mut conn).unwrap();
}

#[test]
fn message_state_keeps_one_row_per_message_and_follows_it() {
    let db = TestDb::new();
    let mut conn = db.connect();
    conn.execute(
        "INSERT INTO accounts (address, profile_id) SELECT 'me@x.com', id FROM profiles LIMIT 1",
        &[],
    ).unwrap();
    let mid: i64 = conn.query_one(
        "INSERT INTO messages (account_id, rfc822_message_id, thread_id)
         SELECT id, '<m1@x>', 'thread:test' FROM accounts RETURNING id",
        &[],
    ).unwrap().get(0);
    conn.execute(
        "INSERT INTO message_state (message_id, state, changed_by) VALUES ($1, 'later', 'me@x.com')",
        &[&mid],
    ).unwrap();

    // one state per message
    let dup = conn.execute(
        "INSERT INTO message_state (message_id, state, changed_by) VALUES ($1, 'handled', 'x')",
        &[&mid],
    );
    assert!(dup.is_err()); // unique violation

    // only handled and later are allowed
    conn.execute("BEGIN", &[]).unwrap();
    let bad = conn.execute("UPDATE message_state SET state = 'archived' WHERE message_id = $1", &[&mid]);
    assert!(bad.is_err()); // check violation
    conn.execute("ROLLBACK", &[]).unwrap();

    let rows: Vec<String> = conn
        .query("SELECT state FROM message_state", &[])
        .unwrap()
        .iter()
        .map(|r| r.get(0))
        .collect();
    assert_eq!(rows, vec!["later"]);

    // the state goes with the message
    conn.execute("DELETE FROM messages WHERE id = $1", &[&mid]).unwrap();
    let n: i64 = conn.query_one("SELECT count(*) FROM message_state", &[]).unwrap().get(0);
    assert_eq!(n, 0);
}

#[test]
fn migrate_runs_on_a_fresh_dsn_via_string() {
    // db.migrate(dsn) over a fresh database, like the Python fixture does
    let fresh = TestDb::new_no_migrate();
    let applied = db::migrate(&fresh.url(), &aimap::logs::silent_logger("aimap.db")).unwrap();
    let names: Vec<String> = db::migrations().unwrap().into_iter().map(|m| m.name).collect();
    assert_eq!(applied, names);
}
